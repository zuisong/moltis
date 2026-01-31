use std::{net::SocketAddr, sync::Arc};

#[cfg(feature = "web-ui")]
use axum::response::Html;
use {
    axum::{
        Router,
        extract::{ConnectInfo, State, WebSocketUpgrade},
        response::{IntoResponse, Json},
        routing::get,
    },
    tower_http::cors::{Any, CorsLayer},
    tracing::info,
};

#[cfg(feature = "web-ui")]
use axum::http::StatusCode;

use moltis_protocol::TICK_INTERVAL_MS;

use moltis_agents::providers::ProviderRegistry;

use moltis_tools::approval::ApprovalManager;

use moltis_sessions::{metadata::SessionMetadata, store::SessionStore};

use crate::{
    approval::{GatewayApprovalBroadcaster, LiveExecApprovalService},
    auth,
    broadcast::broadcast_tick,
    chat::{LiveChatService, LiveModelService},
    methods::MethodRegistry,
    provider_setup::LiveProviderSetupService,
    services::GatewayServices,
    session::LiveSessionService,
    state::GatewayState,
    ws::handle_connection,
};

// ── Shared app state ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    gateway: Arc<GatewayState>,
    methods: Arc<MethodRegistry>,
}

// ── Server startup ───────────────────────────────────────────────────────────

/// Build the gateway router (shared between production startup and tests).
pub fn build_gateway_app(state: Arc<GatewayState>, methods: Arc<MethodRegistry>) -> Router {
    let app_state = AppState {
        gateway: state,
        methods,
    };

    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let router = Router::new()
        .route("/health", get(health_handler))
        .route("/ws", get(ws_upgrade_handler));

    #[cfg(feature = "web-ui")]
    let router = router
        .route("/assets/style.css", get(css_handler))
        .route("/assets/app.js", get(js_handler))
        .fallback(spa_fallback);

    router.layer(cors).with_state(app_state)
}

/// Start the gateway HTTP + WebSocket server.
pub async fn start_gateway(bind: &str, port: u16) -> anyhow::Result<()> {
    // Resolve auth from environment (MOLTIS_TOKEN / MOLTIS_PASSWORD).
    let token = std::env::var("MOLTIS_TOKEN").ok();
    let password = std::env::var("MOLTIS_PASSWORD").ok();
    let resolved_auth = auth::resolve_auth(token, password);

    // Load config file (moltis.toml / .yaml / .json) if present.
    let config = moltis_config::discover_and_load();

    // Merge any previously saved API keys into the provider config so they
    // survive gateway restarts without requiring env vars.
    let key_store = crate::provider_setup::KeyStore::new();
    let effective_providers =
        crate::provider_setup::config_with_saved_keys(&config.providers, &key_store);

    // Discover LLM providers from env + config + saved keys.
    let registry = Arc::new(tokio::sync::RwLock::new(
        ProviderRegistry::from_env_with_config(&effective_providers),
    ));
    let provider_summary = registry.read().await.provider_summary();

    // Create shared approval manager.
    let approval_manager = Arc::new(ApprovalManager::default());

    let mut services = GatewayServices::noop();
    services.exec_approval = Arc::new(LiveExecApprovalService::new(Arc::clone(&approval_manager)));
    services.provider_setup = Arc::new(LiveProviderSetupService::new(
        Arc::clone(&registry),
        config.providers.clone(),
    ));
    if !registry.read().await.is_empty() {
        services = services.with_model(Arc::new(LiveModelService::new(Arc::clone(&registry))));
    }

    // Initialize session storage.
    let sessions_dir = directories::ProjectDirs::from("", "", "moltis")
        .map(|d| d.data_dir().join("sessions"))
        .unwrap_or_else(|| std::path::PathBuf::from(".moltis/sessions"));
    let session_store = Arc::new(SessionStore::new(sessions_dir.clone()));
    let metadata_path = sessions_dir.join("metadata.json");
    let session_metadata = Arc::new(tokio::sync::RwLock::new(
        SessionMetadata::load(metadata_path).unwrap_or_else(|e| {
            tracing::warn!("failed to load session metadata: {e}, starting fresh");
            // Create empty metadata — load won't fail on a non-existent file,
            // so this error means the file was corrupt. Start fresh.
            SessionMetadata::load(sessions_dir.join("metadata_fallback.json")).unwrap()
        }),
    ));

    // Wire live session service.
    services.session = Arc::new(LiveSessionService::new(
        Arc::clone(&session_store),
        Arc::clone(&session_metadata),
    ));

    // Initialize cron service with file-backed store.
    let cron_store: Arc<dyn moltis_cron::store::CronStore> =
        match moltis_cron::store_file::FileStore::default_path() {
            Ok(fs) => Arc::new(fs),
            Err(e) => {
                tracing::warn!("cron file store unavailable ({e}), using in-memory");
                Arc::new(moltis_cron::store_memory::InMemoryStore::new())
            }
        };

    // Deferred reference: populated once GatewayState is ready.
    let deferred_state: Arc<tokio::sync::OnceCell<Arc<GatewayState>>> =
        Arc::new(tokio::sync::OnceCell::new());

    // System event: inject text into the main session and trigger an agent response.
    let sys_state = Arc::clone(&deferred_state);
    let on_system_event: moltis_cron::service::SystemEventFn = Arc::new(move |text| {
        let st = Arc::clone(&sys_state);
        tokio::spawn(async move {
            if let Some(state) = st.get() {
                let chat = state.chat().await;
                let params = serde_json::json!({ "text": text });
                if let Err(e) = chat.send(params).await {
                    tracing::error!("cron system event failed: {e}");
                }
            }
        });
    });

    // Agent turn: run an isolated LLM turn (no session history) and return the output.
    let agent_state = Arc::clone(&deferred_state);
    let on_agent_turn: moltis_cron::service::AgentTurnFn = Arc::new(move |req| {
        let st = Arc::clone(&agent_state);
        Box::pin(async move {
            let state = st
                .get()
                .ok_or_else(|| anyhow::anyhow!("gateway not ready"))?;
            let chat = state.chat().await;
            // Send into an isolated session keyed by a unique id so it doesn't
            // pollute the main conversation.
            let session_key = format!("cron:{}", uuid::Uuid::new_v4());
            let params = serde_json::json!({
                "text": req.message,
                "_session_key": session_key,
            });
            chat.send(params).await.map_err(|e| anyhow::anyhow!(e))?;
            Ok("agent turn dispatched".into())
        })
    });

    let cron_service = moltis_cron::service::CronService::new(
        cron_store,
        on_system_event,
        on_agent_turn,
    );

    // Wire cron into gateway services.
    let live_cron = Arc::new(crate::cron::LiveCronService::new(Arc::clone(&cron_service)));
    services = services.with_cron(live_cron);

    let state = GatewayState::new(resolved_auth, services, Arc::clone(&approval_manager));
    // Populate the deferred reference so cron callbacks can reach the gateway.
    let _ = deferred_state.set(Arc::clone(&state));

    // Wire live chat service (needs state reference, so done after state creation).
    if !registry.read().await.is_empty() {
        let broadcaster = Arc::new(GatewayApprovalBroadcaster::new(Arc::clone(&state)));
        let exec_tool = moltis_tools::exec::ExecTool::default()
            .with_approval(Arc::clone(&approval_manager), broadcaster);

        let cron_tool = moltis_tools::cron_tool::CronTool::new(Arc::clone(&cron_service));

        let mut tool_registry = moltis_agents::tool_registry::ToolRegistry::new();
        tool_registry.register(Box::new(exec_tool));
        tool_registry.register(Box::new(cron_tool));
        let live_chat = Arc::new(
            LiveChatService::new(
                Arc::clone(&registry),
                Arc::clone(&state),
                Arc::clone(&session_store),
                Arc::clone(&session_metadata),
            )
            .with_tools(tool_registry),
        );
        state.set_chat(live_chat).await;
    }

    let methods = Arc::new(MethodRegistry::new());

    let app = build_gateway_app(Arc::clone(&state), Arc::clone(&methods));

    let addr: SocketAddr = format!("{bind}:{port}").parse()?;
    let listener = tokio::net::TcpListener::bind(addr).await?;

    // Startup banner.
    let lines = [
        format!("moltis gateway v{}", state.version),
        format!(
            "protocol v{}, listening on {}",
            moltis_protocol::PROTOCOL_VERSION,
            addr
        ),
        format!("{} methods registered", methods.method_names().len()),
        format!("llm: {}", provider_summary),
    ];
    let width = lines.iter().map(|l| l.len()).max().unwrap_or(0) + 4;
    info!("┌{}┐", "─".repeat(width));
    for line in &lines {
        info!("│  {:<w$}│", line, w = width - 2);
    }
    info!("└{}┘", "─".repeat(width));

    // Spawn tick timer.
    let tick_state = Arc::clone(&state);
    tokio::spawn(async move {
        let mut interval =
            tokio::time::interval(std::time::Duration::from_millis(TICK_INTERVAL_MS));
        loop {
            interval.tick().await;
            broadcast_tick(&tick_state).await;
        }
    });

    // Start the cron scheduler (loads persisted jobs, arms the timer).
    if let Err(e) = cron_service.start().await {
        tracing::warn!("failed to start cron scheduler: {e}");
    }

    // Run the server with ConnectInfo for remote IP extraction.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}

// ── Handlers ─────────────────────────────────────────────────────────────────

async fn health_handler(State(state): State<AppState>) -> impl IntoResponse {
    let count = state.gateway.client_count().await;
    Json(serde_json::json!({
        "status": "ok",
        "version": state.gateway.version,
        "protocol": moltis_protocol::PROTOCOL_VERSION,
        "connections": count,
    }))
}

async fn ws_upgrade_handler(
    ws: WebSocketUpgrade,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    State(state): State<AppState>,
) -> impl IntoResponse {
    ws.on_upgrade(move |socket| handle_connection(socket, state.gateway, state.methods, addr))
}

/// SPA fallback: serve `index.html` for any path not matched by an explicit
/// route (assets, ws, health). This lets client-side routing handle `/crons`,
/// `/methods`, etc.
#[cfg(feature = "web-ui")]
async fn spa_fallback(uri: axum::http::Uri) -> impl IntoResponse {
    // Reject requests that look like missing asset files so the browser gets
    // a proper 404 instead of HTML.
    let path = uri.path();
    if path.starts_with("/assets/") || path.contains('.') {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    Html(include_str!("assets/index.html")).into_response()
}

#[cfg(feature = "web-ui")]
async fn css_handler() -> impl IntoResponse {
    (
        [("content-type", "text/css; charset=utf-8")],
        include_str!("assets/style.css"),
    )
}

#[cfg(feature = "web-ui")]
async fn js_handler() -> impl IntoResponse {
    (
        [("content-type", "application/javascript; charset=utf-8")],
        include_str!("assets/app.js"),
    )
}
