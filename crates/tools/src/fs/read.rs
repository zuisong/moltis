//! `Read` tool — typed, line-numbered file reads.
//!
//! Matches Claude Code's `Read` tool schema: `file_path`, optional `offset`
//! and `limit`. Returns a structured payload with `content` (cat -n style),
//! `total_lines`, and `truncated` flags so the LLM can tell whether it has
//! the full file.

use {
    async_trait::async_trait,
    moltis_agents::tool_registry::AgentTool,
    serde_json::{Value, json},
    tokio::fs,
    tracing::instrument,
};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, labels, tools as tools_metrics};

use crate::{
    Result,
    error::Error,
    fs::shared::{
        DEFAULT_MAX_READ_BYTES, DEFAULT_READ_LINE_LIMIT, canonicalize_existing,
        ensure_regular_file, format_numbered_lines, looks_binary,
    },
};

/// Native `Read` tool implementation.
#[derive(Default)]
pub struct ReadTool;

impl ReadTool {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    #[instrument(skip(self), fields(file_path = %file_path))]
    async fn read_impl(&self, file_path: &str, offset: usize, limit: usize) -> Result<Value> {
        let canonical = canonicalize_existing(file_path).await?;
        let size = ensure_regular_file(&canonical).await?;

        if size > DEFAULT_MAX_READ_BYTES {
            return Err(Error::message(format!(
                "file is too large ({:.1} MB) — maximum is {:.0} MB",
                size as f64 / (1024.0 * 1024.0),
                DEFAULT_MAX_READ_BYTES as f64 / (1024.0 * 1024.0),
            )));
        }

        let bytes = fs::read(&canonical)
            .await
            .map_err(|e| Error::message(format!("failed to read '{file_path}': {e}")))?;

        if looks_binary(&bytes) {
            #[cfg(feature = "metrics")]
            counter!(
                tools_metrics::EXECUTIONS_TOTAL,
                labels::TOOL => "Read".to_string(),
                labels::SUCCESS => "binary".to_string()
            )
            .increment(1);
            return Ok(json!({
                "kind": "binary",
                "file_path": canonical.to_string_lossy(),
                "bytes": bytes.len(),
                "message": "file appears to be binary; content not returned",
            }));
        }

        // Lossy decode so we never fail on invalid UTF-8 — we surface the
        // bytes the LLM can see and let it decide. A stricter mode can come
        // later via config.
        let text = String::from_utf8(bytes).unwrap_or_else(|e| {
            let bytes = e.into_bytes();
            String::from_utf8_lossy(&bytes).into_owned()
        });

        let rendered = format_numbered_lines(&text, offset, limit);

        #[cfg(feature = "metrics")]
        counter!(
            tools_metrics::EXECUTIONS_TOTAL,
            labels::TOOL => "Read".to_string(),
            labels::SUCCESS => "true".to_string()
        )
        .increment(1);

        Ok(json!({
            "kind": "text",
            "file_path": canonical.to_string_lossy(),
            "content": rendered.text,
            "total_lines": rendered.total_lines,
            "start_line": rendered.start_line,
            "rendered_lines": rendered.rendered_lines,
            "truncated": rendered.truncated,
        }))
    }
}

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "Read"
    }

    fn description(&self) -> &str {
        "Read a file from the local filesystem with line-numbered output. \
         Supports `offset` (1-indexed line to start at) and `limit` (max lines \
         to return) for paginating large files. Returns structured JSON with \
         the file's content, total line count, and truncation flag. Binary \
         files return a typed marker instead of garbage."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["file_path"],
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to read."
                },
                "offset": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "1-indexed line number to start reading from."
                },
                "limit": {
                    "type": "integer",
                    "minimum": 1,
                    "description": "Maximum number of lines to return (default 2000)."
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let file_path = params
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'file_path' parameter"))?;
        let offset = params
            .get("offset")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(1)
            .max(1);
        let limit = params
            .get("limit")
            .and_then(Value::as_u64)
            .map(|v| v as usize)
            .unwrap_or(DEFAULT_READ_LINE_LIMIT)
            .max(1);

        match self.read_impl(file_path, offset, limit).await {
            Ok(value) => Ok(value),
            Err(e) => {
                #[cfg(feature = "metrics")]
                counter!(
                    tools_metrics::EXECUTION_ERRORS_TOTAL,
                    labels::TOOL => "Read".to_string()
                )
                .increment(1);
                Err(e.into())
            },
        }
    }
}

#[allow(clippy::unwrap_used, clippy::expect_used)]
#[cfg(test)]
mod tests {
    use {super::*, std::io::Write};

    #[tokio::test]
    async fn read_small_text_file() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(b"line one\nline two\nline three\n").unwrap();

        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": tmp.path().to_str().unwrap() }))
            .await
            .unwrap();

        assert_eq!(value["kind"], "text");
        assert_eq!(value["total_lines"], 3);
        assert_eq!(value["rendered_lines"], 3);
        assert_eq!(value["truncated"], false);
        assert!(value["content"].as_str().unwrap().contains("→line one"));
    }

    #[tokio::test]
    async fn read_paginated() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        for i in 1..=10 {
            writeln!(tmp, "line {i}").unwrap();
        }

        let tool = ReadTool::new();
        let value = tool
            .execute(json!({
                "file_path": tmp.path().to_str().unwrap(),
                "offset": 3,
                "limit": 2,
            }))
            .await
            .unwrap();

        assert_eq!(value["total_lines"], 10);
        assert_eq!(value["rendered_lines"], 2);
        assert_eq!(value["start_line"], 3);
        assert_eq!(value["truncated"], true);
        let content = value["content"].as_str().unwrap();
        assert!(content.contains("line 3"));
        assert!(content.contains("line 4"));
        assert!(!content.contains("line 5"));
    }

    #[tokio::test]
    async fn read_binary_returns_typed_marker() {
        let mut tmp = tempfile::NamedTempFile::new().unwrap();
        tmp.write_all(&[0u8, 1, 2, 3, 0, 4, 5]).unwrap();

        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": tmp.path().to_str().unwrap() }))
            .await
            .unwrap();

        assert_eq!(value["kind"], "binary");
        assert_eq!(value["bytes"], 7);
    }

    #[tokio::test]
    async fn read_missing_file_errors() {
        let tool = ReadTool::new();
        let err = tool
            .execute(json!({ "file_path": "/tmp/does-not-exist-read-test-xyz-123" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot resolve path"));
    }

    #[tokio::test]
    async fn read_rejects_directory() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadTool::new();
        let err = tool
            .execute(json!({ "file_path": dir.path().to_str().unwrap() }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("not a regular file"));
    }

    #[tokio::test]
    async fn read_missing_file_path_errors() {
        let tool = ReadTool::new();
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("missing 'file_path'"));
    }
}
