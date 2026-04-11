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
        DEFAULT_MAX_READ_BYTES, DEFAULT_READ_LINE_LIMIT, FsErrorKind, FsState, READ_LOOP_THRESHOLD,
        format_numbered_lines, fs_error_payload, io_error_to_typed_payload, looks_binary,
        require_absolute, session_key_from,
    },
};

/// Native `Read` tool implementation.
#[derive(Default)]
pub struct ReadTool {
    fs_state: Option<FsState>,
}

impl ReadTool {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach a shared [`FsState`] for per-session read tracking and
    /// re-read loop detection. When set, each successful Read records the
    /// file and, if the same `(path, offset, limit)` repeats beyond
    /// [`READ_LOOP_THRESHOLD`] times without an intervening mutation, the
    /// response payload gains a `loop_warning` field.
    #[must_use]
    pub fn with_fs_state(mut self, state: FsState) -> Self {
        self.fs_state = Some(state);
        self
    }

    #[instrument(skip(self), fields(file_path = %file_path))]
    async fn read_impl(
        &self,
        file_path: &str,
        offset: usize,
        limit: usize,
        session_key: &str,
    ) -> Result<Value> {
        require_absolute(file_path, "file_path")?;

        // Stat first so we can surface not_found / permission_denied as
        // typed Ok payloads rather than Err strings. The chat loop strips
        // Err detail via err.to_string(); typed payloads survive as JSON.
        let meta = match fs::metadata(file_path).await {
            Ok(m) => m,
            Err(e) => {
                if let Some(payload) = io_error_to_typed_payload(&e, file_path) {
                    return Ok(payload);
                }
                return Err(Error::message(format!("cannot stat '{file_path}': {e}")));
            },
        };

        if !meta.is_file() {
            return Ok(fs_error_payload(
                FsErrorKind::NotRegularFile,
                file_path,
                "path is not a regular file",
                None,
            ));
        }

        let size = meta.len();
        if size > DEFAULT_MAX_READ_BYTES {
            return Ok(json!({
                "kind": FsErrorKind::TooLarge.as_str(),
                "file_path": file_path,
                "error": format!(
                    "file is too large ({:.1} MB) — maximum is {:.0} MB",
                    size as f64 / (1024.0 * 1024.0),
                    DEFAULT_MAX_READ_BYTES as f64 / (1024.0 * 1024.0),
                ),
                "bytes": size,
                "max_bytes": DEFAULT_MAX_READ_BYTES,
            }));
        }

        let bytes = match fs::read(file_path).await {
            Ok(b) => b,
            Err(e) => {
                if let Some(payload) = io_error_to_typed_payload(&e, file_path) {
                    return Ok(payload);
                }
                return Err(Error::message(format!("failed to read '{file_path}': {e}")));
            },
        };

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
                "file_path": file_path,
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

        let mut payload = json!({
            "kind": "text",
            "file_path": file_path,
            "content": rendered.text,
            "total_lines": rendered.total_lines,
            "start_line": rendered.start_line,
            "rendered_lines": rendered.rendered_lines,
            "truncated": rendered.truncated,
        });

        // Record in the shared tracker if one is configured. Emit a
        // `loop_warning` when the LLM is re-reading the same slice after
        // context compression without doing any intervening work.
        //
        // Canonicalize first so Write/Edit's subsequent `has_been_read`
        // check (which also canonicalizes) compares against the same
        // resolved path. On macOS `/tmp` is a symlink to `/private/tmp`,
        // so raw vs canonical disagree.
        if let Some(ref state) = self.fs_state {
            let canonical_path = fs::canonicalize(file_path)
                .await
                .unwrap_or_else(|_| std::path::PathBuf::from(file_path));
            let mut guard = state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let consecutive = guard.record_read(session_key, canonical_path, offset, limit);
            if consecutive >= READ_LOOP_THRESHOLD
                && let Some(obj) = payload.as_object_mut()
            {
                obj.insert(
                    "loop_warning".into(),
                    json!(format!(
                        "This exact read (file_path={file_path}, offset={offset}, limit={limit}) \
                         has been repeated {consecutive} times with no intervening edit. The \
                         file hasn't changed — stop re-reading it and make progress on the task."
                    )),
                );
            }
        }

        Ok(payload)
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
        let session_key = session_key_from(&params).to_string();

        match self.read_impl(file_path, offset, limit, &session_key).await {
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
    async fn read_missing_file_returns_typed_not_found() {
        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": "/tmp/does-not-exist-read-test-xyz-123" }))
            .await
            .unwrap();
        assert_eq!(value["kind"], "not_found");
        assert_eq!(value["file_path"], "/tmp/does-not-exist-read-test-xyz-123");
    }

    #[tokio::test]
    async fn read_directory_returns_typed_not_regular_file() {
        let dir = tempfile::tempdir().unwrap();
        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": dir.path().to_str().unwrap() }))
            .await
            .unwrap();
        assert_eq!(value["kind"], "not_regular_file");
    }

    #[tokio::test]
    async fn read_too_large_returns_typed_payload() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("big.bin");
        let f = std::fs::File::create(&path).unwrap();
        // One byte past the cap.
        f.set_len(DEFAULT_MAX_READ_BYTES + 1).unwrap();

        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": path.to_str().unwrap() }))
            .await
            .unwrap();
        assert_eq!(value["kind"], "too_large");
        assert!(value["bytes"].as_u64().unwrap() > DEFAULT_MAX_READ_BYTES);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn read_permission_denied_returns_typed_payload() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secret.txt");
        fs::write(&path, "secret").await.unwrap();
        let mut perms = fs::metadata(&path).await.unwrap().permissions();
        perms.set_mode(0o000);
        fs::set_permissions(&path, perms).await.unwrap();

        let tool = ReadTool::new();
        let value = tool
            .execute(json!({ "file_path": path.to_str().unwrap() }))
            .await
            .unwrap();

        // Root bypasses permission checks; tolerate either typed error
        // or a successful text read so the test is CI-safe.
        let kind = value["kind"].as_str().unwrap();
        assert!(
            kind == "permission_denied" || kind == "text",
            "unexpected kind: {kind}"
        );

        // Restore perms so tempdir cleanup works.
        let mut restore = fs::metadata(&path).await.unwrap().permissions();
        restore.set_mode(0o644);
        let _ = fs::set_permissions(&path, restore).await;
    }

    #[tokio::test]
    async fn read_relative_path_errors() {
        let tool = ReadTool::new();
        let err = tool
            .execute(json!({ "file_path": "relative.txt" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must be an absolute path"));
    }

    #[tokio::test]
    async fn read_missing_file_path_errors() {
        let tool = ReadTool::new();
        let err = tool.execute(json!({})).await.unwrap_err();
        assert!(err.to_string().contains("missing 'file_path'"));
    }
}
