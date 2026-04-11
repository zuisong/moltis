//! `Write` tool — atomic file writes.
//!
//! Writes the full contents of a file via a temp-file + rename sequence so
//! partial writes are never visible. Refuses to follow symlinks by default.

use {
    async_trait::async_trait,
    moltis_agents::tool_registry::AgentTool,
    serde_json::{Value, json},
    std::io::Write as _,
    tracing::instrument,
};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, labels, tools as tools_metrics};

use crate::{
    Result,
    error::Error,
    fs::shared::{canonicalize_for_create, reject_if_symlink},
};

/// Native `Write` tool implementation.
#[derive(Default)]
pub struct WriteTool;

impl WriteTool {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    #[instrument(skip(self, content), fields(file_path = %file_path, bytes = content.len()))]
    async fn write_impl(&self, file_path: &str, content: &str) -> Result<Value> {
        let canonical = canonicalize_for_create(file_path).await?;

        // If the target exists, reject symlinks so we don't unknowingly write
        // through to another location. A new file naturally isn't a symlink.
        if tokio::fs::try_exists(&canonical).await.unwrap_or(false) {
            reject_if_symlink(
                canonical
                    .to_str()
                    .ok_or_else(|| Error::message("file_path contains invalid UTF-8"))?,
            )
            .await?;
        }

        let parent = canonical
            .parent()
            .ok_or_else(|| Error::message(format!("'{file_path}' has no parent directory")))?;

        let bytes = content.as_bytes().to_vec();
        let canonical_for_blocking = canonical.clone();
        let parent_owned = parent.to_path_buf();

        // tempfile's persist + sync on a blocking thread so we stay async-safe.
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut tmp = tempfile::NamedTempFile::new_in(&parent_owned).map_err(|e| {
                Error::message(format!(
                    "failed to create temp file in '{}': {e}",
                    parent_owned.display()
                ))
            })?;
            tmp.write_all(&bytes)
                .map_err(|e| Error::message(format!("failed to write temp file: {e}")))?;
            tmp.as_file()
                .sync_all()
                .map_err(|e| Error::message(format!("failed to fsync temp file: {e}")))?;
            tmp.persist(&canonical_for_blocking).map_err(|e| {
                Error::message(format!(
                    "failed to persist file '{}': {e}",
                    canonical_for_blocking.display()
                ))
            })?;
            Ok(())
        })
        .await
        .map_err(|e| Error::message(format!("blocking write task failed: {e}")))??;

        #[cfg(feature = "metrics")]
        counter!(
            tools_metrics::EXECUTIONS_TOTAL,
            labels::TOOL => "Write".to_string(),
            labels::SUCCESS => "true".to_string()
        )
        .increment(1);

        Ok(json!({
            "file_path": canonical.to_string_lossy(),
            "bytes_written": content.len(),
        }))
    }
}

#[async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "Write"
    }

    fn description(&self) -> &str {
        "Atomically write a file to the local filesystem. Parent directories \
         must already exist. Refuses to follow symlinks. The entire file is \
         replaced — use `Edit` for surgical changes."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["file_path", "content"],
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to write."
                },
                "content": {
                    "type": "string",
                    "description": "Full file contents to write."
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let file_path = params
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'file_path' parameter"))?;
        let content = params
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'content' parameter"))?;

        match self.write_impl(file_path, content).await {
            Ok(value) => Ok(value),
            Err(e) => {
                #[cfg(feature = "metrics")]
                counter!(
                    tools_metrics::EXECUTION_ERRORS_TOTAL,
                    labels::TOOL => "Write".to_string()
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
    use super::*;

    #[tokio::test]
    async fn write_creates_new_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");

        let tool = WriteTool::new();
        let value = tool
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "content": "hello world\n",
            }))
            .await
            .unwrap();

        assert_eq!(value["bytes_written"], 12);
        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "hello world\n");
    }

    #[tokio::test]
    async fn write_overwrites_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("out.txt");
        tokio::fs::write(&target, "old contents").await.unwrap();

        let tool = WriteTool::new();
        tool.execute(json!({
            "file_path": target.to_str().unwrap(),
            "content": "new contents",
        }))
        .await
        .unwrap();

        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "new contents");
    }

    #[tokio::test]
    async fn write_rejects_symlink_target() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        tokio::fs::write(&real, "real").await.unwrap();
        let link = dir.path().join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        {
            // Symlink creation on windows requires privileges; skip.
            return;
        }

        let tool = WriteTool::new();
        let err = tool
            .execute(json!({
                "file_path": link.to_str().unwrap(),
                "content": "nope",
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("symbolic link"));
    }

    #[tokio::test]
    async fn write_missing_parent_errors() {
        let tool = WriteTool::new();
        let err = tool
            .execute(json!({
                "file_path": "/tmp/definitely-missing-parent-ab91x/out.txt",
                "content": "x",
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("cannot resolve parent"));
    }

    #[tokio::test]
    async fn write_missing_content_errors() {
        let tool = WriteTool::new();
        let err = tool
            .execute(json!({ "file_path": "/tmp/x" }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("missing 'content'"));
    }
}
