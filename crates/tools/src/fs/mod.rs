//! Native filesystem tools: `Read`, `Write`, `Edit`, `MultiEdit`, `Glob`, `Grep`.
//!
//! These are the structured, typed alternative to shell-based file I/O via
//! `exec`. They match Claude Code's tool schemas exactly so LLMs trained on
//! those tools encounter the same shape of parameters and responses.
//!
//! See GH moltis-org/moltis#657 for context.
//!
//! Phase 1 (this module) covers host-path execution only. Sandbox routing
//! arrives in phase 2, UX polish (adaptive paging, edit recovery, re-read
//! detection) in phase 3, and operator-facing `[tools.fs]` config in phase 4.

pub mod edit;
pub mod glob;
pub mod grep;
pub mod multi_edit;
pub mod read;
pub mod shared;
pub mod write;

pub use {
    edit::EditTool, glob::GlobTool, grep::GrepTool, multi_edit::MultiEditTool, read::ReadTool,
    write::WriteTool,
};

use moltis_agents::tool_registry::ToolRegistry;

/// Register every native filesystem tool on a [`ToolRegistry`].
///
/// Intended to be called from the gateway's tool-registration pipeline. The
/// `tools.policy` allow/deny layer still gates access per-agent, so
/// registration is independent of authorization.
pub fn register_fs_tools(registry: &mut ToolRegistry) {
    registry.register(Box::new(ReadTool::new()));
    registry.register(Box::new(WriteTool::new()));
    registry.register(Box::new(EditTool::new()));
    registry.register(Box::new(MultiEditTool::new()));
    registry.register(Box::new(GlobTool::new()));
    registry.register(Box::new(GrepTool::new()));
}
