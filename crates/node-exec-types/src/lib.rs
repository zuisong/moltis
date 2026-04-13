//! Core types and constants for node execution.
//!
//! This crate contains the shared types and constants used by the gateway
//! and other crates for remote node execution.

use {
    serde::{Deserialize, Serialize},
    std::collections::HashMap,
};

// Re-export the async_trait macro for trait implementations
pub use async_trait::async_trait;

/// Result of a remote command execution on a node.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeExecResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
}

/// Environment variables that are safe to forward to a remote node.
pub const SAFE_ENV_ALLOWLIST: &[&str] = &["TERM", "LANG", "COLORTERM", "NO_COLOR", "FORCE_COLOR"];

/// Environment variable prefixes that are safe to forward.
pub const SAFE_ENV_PREFIX_ALLOWLIST: &[&str] = &["LC_"];

/// Environment variable patterns that must NEVER be forwarded to a remote node.
pub const BLOCKED_ENV_PREFIXES: &[&str] = &[
    "DYLD_",
    "LD_",
    "NODE_OPTIONS",
    "PYTHON",
    "PERL",
    "RUBYOPT",
    "SHELLOPTS",
    "PS4",
    // Security-sensitive keys
    "MOLTIS_",
    "OPENAI_",
    "ANTHROPIC_",
    "AWS_",
    "GOOGLE_",
    "AZURE_",
];

/// SSH node ID prefix.
pub const SSH_ID_PREFIX: &str = "ssh:";

/// SSH target ID prefix.
pub const SSH_TARGET_ID_PREFIX: &str = "ssh:target:";

/// Generate a node ID for an SSH target.
pub fn ssh_node_id(target: &str) -> String {
    format!("{SSH_ID_PREFIX}{target}")
}

/// Generate a stored node ID from a database ID.
pub fn ssh_stored_node_id(id: i64) -> String {
    format!("{SSH_TARGET_ID_PREFIX}{id}")
}

/// Check if a node reference matches an SSH target.
pub fn ssh_target_matches(node_ref: &str, target: &str) -> bool {
    node_ref == "ssh" || node_ref == target || node_ref.strip_prefix(SSH_ID_PREFIX) == Some(target)
}

/// Filter environment variables to only include safe ones.
pub fn filter_env(env: &HashMap<String, String>) -> HashMap<String, String> {
    env.iter()
        .filter(|(key, _)| is_safe_env(key) && is_valid_env_key(key))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect()
}

/// Check if an environment variable key is safe to forward.
pub fn is_safe_env(key: &str) -> bool {
    // Block dangerous prefixes first.
    for prefix in BLOCKED_ENV_PREFIXES {
        if key.starts_with(prefix) {
            return false;
        }
    }

    // Allow exact matches.
    if SAFE_ENV_ALLOWLIST.contains(&key) {
        return true;
    }

    // Allow prefix matches.
    for prefix in SAFE_ENV_PREFIX_ALLOWLIST {
        if key.starts_with(prefix) {
            return true;
        }
    }

    false
}

/// Check if an environment variable key has valid format.
pub fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    matches!(chars.next(), Some(ch) if ch.is_ascii_alphabetic() || ch == '_')
        && chars.all(|ch| ch.is_ascii_alphanumeric() || ch == '_')
}

// ── Node info types ─────────────────────────────────────────────────────────

/// Serializable summary of a connected node, returned by the list/describe tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NodeInfo {
    pub node_id: String,
    pub display_name: Option<String>,
    pub platform: String,
    pub capabilities: Vec<String>,
    pub commands: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remote_ip: Option<String>,
    // Telemetry
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mem_available: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cpu_usage: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uptime_secs: Option<u64>,
    pub services: Vec<String>,
    pub telemetry_stale: bool,
    // P1 fields (populated when provider discovery / richer telemetry lands)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_total: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub disk_available: Option<u64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub runtimes: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub providers: Vec<NodeProviderInfo>,
}

/// A provider discovered on a remote node (e.g. ollama, openai env key).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeProviderInfo {
    pub provider: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub models: Vec<String>,
}

// ── Provider trait ──────────────────────────────────────────────────────────

/// Abstraction that the gateway implements to supply node data to the tools
/// crate without a direct dependency.
///
/// Follows the same pattern as [`crate::exec::NodeExecProvider`].
#[async_trait]
pub trait NodeInfoProvider: Send + Sync {
    /// List all currently connected nodes.
    async fn list_nodes(&self) -> Vec<NodeInfo>;

    /// Describe a single node by id or display name.
    async fn describe_node(&self, node_ref: &str) -> Option<NodeInfo>;

    /// Assign (or clear) a node for a chat session.
    /// `node_ref` is an id or display name; `None` clears the assignment.
    async fn set_session_node(
        &self,
        session_key: &str,
        node_ref: Option<&str>,
    ) -> anyhow::Result<Option<String>>;

    /// Resolve a node reference (id or display name) to a canonical node_id.
    async fn resolve_node_id(&self, node_ref: &str) -> Option<String>;
}
