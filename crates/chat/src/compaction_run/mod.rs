//! Compaction strategy dispatcher.
//!
//! Routes a session history through the [`CompactionMode`] selected in
//! `chat.compaction`. Each strategy lives in its own submodule and owns
//! its own tests; shared boundary / pruning / tool-pair helpers live in
//! [`shared`].
//!
//! Submodules:
//! - [`deterministic`] — zero-LLM replace-all extraction.
//! - [`recency_preserving`] — zero-LLM head + middle-marker + tail.
//! - [`structured`] — LLM head + structured-summary + tail (feature-gated).
//! - [`llm_replace`] — LLM streaming-summary replace-all (feature-gated).
//! - [`shared`] — boundary computation, pruning, tool-pair repair,
//!   message builders.
//! - `test_support` — `#[cfg(test)]`-only stub provider for the LLM modes.
//!
//! See `docs/src/compaction.md` for the full mode comparison and trade-off
//! guidance, and the rustdoc on [`moltis_config::CompactionMode`] for
//! per-variant semantics.

use {
    moltis_agents::model::LlmProvider,
    moltis_config::{CompactionConfig, CompactionMode},
    serde_json::Value,
    thiserror::Error,
};

mod deterministic;
mod recency_preserving;
mod shared;

#[cfg(feature = "llm-compaction")]
mod llm_replace;
#[cfg(feature = "llm-compaction")]
mod structured;

#[cfg(test)]
mod test_support;

/// Errors surfaced by [`run_compaction`].
///
/// Several variants are gated on the `llm-compaction` cargo feature;
/// when the feature is off the LLM-backed strategies aren't compiled in,
/// so their dedicated error variants become dead code.
#[derive(Debug, Error)]
pub(crate) enum CompactionRunError {
    /// History was empty — nothing to compact.
    #[error("nothing to compact")]
    EmptyHistory,
    /// The strategy produced no summary text.
    #[error("compact produced empty summary")]
    EmptySummary,
    /// A mode that requires an LLM provider was selected but none was passed.
    #[cfg(feature = "llm-compaction")]
    #[error("compaction mode '{mode}' requires an LLM provider to be available for the session")]
    ProviderRequired { mode: &'static str },
    /// The user selected a mode that requires a cargo feature that isn't enabled.
    #[cfg(not(feature = "llm-compaction"))]
    #[error("compaction mode '{mode}' requires the 'llm-compaction' cargo feature to be enabled")]
    FeatureDisabled { mode: &'static str },
    /// The LLM streaming summary call failed.
    #[cfg(feature = "llm-compaction")]
    #[error("compact summarization failed: {0}")]
    LlmFailed(String),
    /// `recency_preserving` couldn't make meaningful progress: the history
    /// is already smaller than `protect_head + protect_tail_min + 1`, and
    /// there was no bulky tool-result content to prune. The caller should
    /// fall back to a different mode (e.g. `deterministic`) rather than
    /// loop.
    #[error(
        "history has {messages} messages — too small for recency_preserving with \
         protect_head={head} and protect_tail_min={tail}; no tool-result pruning \
         was possible either. Try chat.compaction.mode = \"deterministic\" for \
         tiny sessions."
    )]
    TooSmallToCompact {
        messages: usize,
        head: usize,
        tail: usize,
    },
}

/// Best-effort extraction of a human-readable summary body from a
/// compacted history, for use in memory-file snapshots and hook payloads.
///
/// Walks the compacted messages looking for the first one whose text
/// content begins with either `[Conversation Summary]` (produced by the
/// `deterministic`, `structured`, and `llm_replace` modes) or
/// `[Conversation Compacted]` (produced by the `recency_preserving`
/// middle marker), and returns the stripped body. Returns an empty
/// string when no summary-shaped message is found, which is fine for
/// hook `summary_len` reporting and falls through gracefully.
#[must_use]
pub(crate) fn extract_summary_body(compacted: &[Value]) -> String {
    compacted
        .iter()
        .filter_map(|msg| msg.get("content").and_then(Value::as_str))
        .find_map(|content| {
            content
                .strip_prefix("[Conversation Summary]\n\n")
                .or_else(|| content.strip_prefix("[Conversation Compacted]\n\n"))
                .map(str::to_string)
        })
        .unwrap_or_default()
}

/// Run the compaction strategy selected by `config` against `history`.
///
/// Returns the replacement history vec. Call sites are responsible for
/// writing the result back to the session store.
///
/// `provider` is only consulted by LLM-backed modes; pass `None` when no
/// provider has been resolved for the session. LLM modes return
/// [`CompactionRunError::ProviderRequired`] when called without one (or
/// [`CompactionRunError::FeatureDisabled`] when the `llm-compaction`
/// cargo feature is off).
pub(crate) async fn run_compaction(
    history: &[Value],
    config: &CompactionConfig,
    provider: Option<&dyn LlmProvider>,
) -> Result<Vec<Value>, CompactionRunError> {
    if history.is_empty() {
        return Err(CompactionRunError::EmptyHistory);
    }

    match config.mode {
        CompactionMode::Deterministic => deterministic::run(history),
        CompactionMode::RecencyPreserving => {
            let context_window = provider.map_or(200_000, LlmProvider::context_window);
            recency_preserving::run(history, config, context_window)
        },
        CompactionMode::Structured => {
            #[cfg(feature = "llm-compaction")]
            {
                let provider =
                    provider.ok_or(CompactionRunError::ProviderRequired { mode: "structured" })?;
                let context_window = provider.context_window();
                structured::run(history, config, context_window, provider).await
            }
            #[cfg(not(feature = "llm-compaction"))]
            {
                let _ = (config, provider);
                Err(CompactionRunError::FeatureDisabled { mode: "structured" })
            }
        },
        CompactionMode::LlmReplace => {
            #[cfg(feature = "llm-compaction")]
            {
                let provider = provider.ok_or(CompactionRunError::ProviderRequired {
                    mode: "llm_replace",
                })?;
                llm_replace::run(history, config, provider).await
            }
            #[cfg(not(feature = "llm-compaction"))]
            {
                let _ = (config, provider);
                Err(CompactionRunError::FeatureDisabled {
                    mode: "llm_replace",
                })
            }
        },
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use {super::*, serde_json::json};

    /// Minimal fixture used by the feature-off tests that exercise the
    /// dispatcher's `FeatureDisabled` branches without going through a
    /// strategy implementation.
    #[cfg(not(feature = "llm-compaction"))]
    fn sample_history() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi there"}),
        ]
    }

    // ── Dispatcher ────────────────────────────────────────────────────

    #[tokio::test]
    async fn empty_history_returns_empty_history_error() {
        let config = CompactionConfig::default();
        let err = run_compaction(&[], &config, None).await.unwrap_err();
        assert!(matches!(err, CompactionRunError::EmptyHistory));
    }

    #[cfg(not(feature = "llm-compaction"))]
    #[tokio::test]
    async fn llm_replace_mode_returns_feature_disabled_when_feature_off() {
        let config = CompactionConfig {
            mode: CompactionMode::LlmReplace,
            ..Default::default()
        };
        let err = run_compaction(&sample_history(), &config, None)
            .await
            .unwrap_err();
        match err {
            CompactionRunError::FeatureDisabled { mode } => assert_eq!(mode, "llm_replace"),
            other => panic!("expected FeatureDisabled, got {other:?}"),
        }
    }

    #[cfg(not(feature = "llm-compaction"))]
    #[tokio::test]
    async fn structured_mode_returns_feature_disabled_when_feature_off() {
        let config = CompactionConfig {
            mode: CompactionMode::Structured,
            ..Default::default()
        };
        let err = run_compaction(&sample_history(), &config, None)
            .await
            .unwrap_err();
        match err {
            CompactionRunError::FeatureDisabled { mode } => assert_eq!(mode, "structured"),
            other => panic!("expected FeatureDisabled, got {other:?}"),
        }
    }

    // ── extract_summary_body (memory-file / hook helper) ──────────────

    #[test]
    fn extract_summary_body_finds_conversation_summary_prefix() {
        // deterministic / structured / llm_replace shape: single summary
        // message at index 0.
        let compacted = vec![json!({
            "role": "user",
            "content": "[Conversation Summary]\n\nBody text here",
        })];
        assert_eq!(extract_summary_body(&compacted), "Body text here");
    }

    #[test]
    fn extract_summary_body_finds_conversation_compacted_marker() {
        // recency_preserving shape: head verbatim, then middle marker,
        // then tail verbatim. The marker is NOT at index 0.
        let compacted = vec![
            json!({"role": "user", "content": "first user"}),
            json!({"role": "assistant", "content": "first reply"}),
            json!({
                "role": "user",
                "content": "[Conversation Compacted]\n\n6 earlier messages were elided …",
            }),
            json!({"role": "user", "content": "recent user"}),
            json!({"role": "assistant", "content": "recent reply"}),
        ];
        let body = extract_summary_body(&compacted);
        assert!(
            body.starts_with("6 earlier messages were elided"),
            "got: {body}"
        );
    }

    #[test]
    fn extract_summary_body_returns_empty_when_no_summary_shaped_message_present() {
        // Pathological: history with no summary-shaped message. Helper
        // should return "" rather than picking up unrelated content.
        let compacted = vec![
            json!({"role": "user", "content": "just a regular user turn"}),
            json!({"role": "assistant", "content": "just a regular reply"}),
        ];
        assert_eq!(extract_summary_body(&compacted), "");
    }
}
