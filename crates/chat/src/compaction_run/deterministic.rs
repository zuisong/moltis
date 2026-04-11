//! `CompactionMode::Deterministic` — zero-LLM extraction + replace-all.
//!
//! Wraps the structured-extraction helpers in [`crate::compaction`] and
//! produces a single `[Conversation Summary]` user message that replaces
//! the entire history. Current PR #653 default behaviour.

use {serde_json::Value, tracing::info};

use super::{CompactionRunError, shared::build_summary_message};

/// Run the deterministic extraction strategy against `history`.
///
/// Returns a single-element vec containing the replacement summary
/// message, or [`CompactionRunError::EmptySummary`] if extraction
/// produced no usable text.
pub(super) fn run(history: &[Value]) -> Result<Vec<Value>, CompactionRunError> {
    let merged = crate::compaction::compute_compaction_summary(history)
        .ok_or(CompactionRunError::EmptySummary)?;
    let summary = crate::compaction::compress_summary(&merged);
    if summary.is_empty() {
        return Err(CompactionRunError::EmptySummary);
    }

    info!(
        messages = history.len(),
        "chat.compact: deterministic summary"
    );

    Ok(vec![build_summary_message(
        &crate::compaction::get_compact_continuation_message(&summary, false),
    )])
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use {
        super::*,
        moltis_config::{CompactionConfig, CompactionMode},
        serde_json::json,
    };

    fn sample_history() -> Vec<Value> {
        vec![
            json!({"role": "user", "content": "hello"}),
            json!({"role": "assistant", "content": "hi there"}),
            json!({"role": "user", "content": "what is 2+2"}),
            json!({"role": "assistant", "content": "4"}),
        ]
    }

    #[tokio::test]
    async fn deterministic_mode_returns_single_summary_message() {
        let history = sample_history();
        let config = CompactionConfig {
            mode: CompactionMode::Deterministic,
            ..Default::default()
        };
        let result = super::super::run_compaction(&history, &config, None)
            .await
            .expect("deterministic dispatch succeeds");
        assert_eq!(
            result.len(),
            1,
            "deterministic mode replaces history with one message"
        );
        let text = result[0]
            .get("content")
            .and_then(Value::as_str)
            .expect("summary has string content");
        assert!(
            text.starts_with("[Conversation Summary]\n\n"),
            "summary is wrapped in the expected preamble, got: {text}"
        );
    }
}
