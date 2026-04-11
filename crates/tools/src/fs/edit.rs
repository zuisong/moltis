//! `Edit` tool — exact-match string replacement with uniqueness enforcement.
//!
//! The uniqueness requirement is the main correctness win over shell `sed`:
//! if `old_string` matches more than once, the edit is rejected unless
//! `replace_all=true`. The error payload includes the match count so the LLM
//! can fix its input.

use {
    async_trait::async_trait,
    moltis_agents::tool_registry::AgentTool,
    serde_json::{Value, json},
    std::{io::Write as _, path::Path},
    tracing::instrument,
};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, labels, tools as tools_metrics};

use crate::{
    Result,
    error::Error,
    fs::shared::{canonicalize_existing, ensure_regular_file, reject_if_symlink},
};

/// Apply a single exact-match edit to `content`, enforcing uniqueness semantics.
///
/// Returns the new content on success. Errors surface the number of matches so
/// the caller (and the LLM via the tool result) can tell why the edit failed.
pub(crate) fn apply_edit(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<String> {
    if old_string.is_empty() {
        return Err(Error::message("'old_string' must not be empty"));
    }
    if old_string == new_string {
        return Err(Error::message("'new_string' must differ from 'old_string'"));
    }

    let match_count = content.matches(old_string).count();
    if match_count == 0 {
        return Err(Error::message(
            "'old_string' not found in file — edit refused",
        ));
    }
    if match_count > 1 && !replace_all {
        return Err(Error::message(format!(
            "'old_string' matches {match_count} locations in the file; \
             refusing to edit. Supply a larger `old_string` with more \
             context to make the match unique, or set `replace_all=true`."
        )));
    }

    if replace_all {
        Ok(content.replace(old_string, new_string))
    } else {
        // Replace only the first occurrence (match_count == 1 at this point).
        Ok(content.replacen(old_string, new_string, 1))
    }
}

/// Native `Edit` tool implementation.
#[derive(Default)]
pub struct EditTool;

impl EditTool {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    #[instrument(skip(self, old_string, new_string), fields(file_path = %file_path, replace_all))]
    async fn edit_impl(
        &self,
        file_path: &str,
        old_string: &str,
        new_string: &str,
        replace_all: bool,
    ) -> Result<Value> {
        reject_if_symlink(file_path).await?;
        let canonical = canonicalize_existing(file_path).await?;
        ensure_regular_file(&canonical).await?;

        let content = tokio::fs::read_to_string(&canonical)
            .await
            .map_err(|e| Error::message(format!("failed to read '{file_path}': {e}")))?;

        let replacements_before = content.matches(old_string).count();
        let updated = apply_edit(&content, old_string, new_string, replace_all)?;

        persist_atomic(&canonical, &updated).await?;

        let replacements = if replace_all {
            replacements_before
        } else {
            1
        };

        #[cfg(feature = "metrics")]
        counter!(
            tools_metrics::EXECUTIONS_TOTAL,
            labels::TOOL => "Edit".to_string(),
            labels::SUCCESS => "true".to_string()
        )
        .increment(1);

        Ok(json!({
            "file_path": canonical.to_string_lossy(),
            "replacements": replacements,
            "replace_all": replace_all,
        }))
    }
}

/// Persist `content` to `path` atomically via a sibling temp file + rename.
pub(crate) async fn persist_atomic(path: &Path, content: &str) -> Result<()> {
    let path = path.to_path_buf();
    let parent = path
        .parent()
        .ok_or_else(|| Error::message(format!("'{}' has no parent", path.display())))?
        .to_path_buf();
    let bytes = content.as_bytes().to_vec();

    tokio::task::spawn_blocking(move || -> Result<()> {
        let mut tmp = tempfile::NamedTempFile::new_in(&parent).map_err(|e| {
            Error::message(format!(
                "failed to create temp file in '{}': {e}",
                parent.display()
            ))
        })?;
        tmp.write_all(&bytes)
            .map_err(|e| Error::message(format!("failed to write temp file: {e}")))?;
        tmp.as_file()
            .sync_all()
            .map_err(|e| Error::message(format!("failed to fsync temp file: {e}")))?;
        tmp.persist(&path)
            .map_err(|e| Error::message(format!("failed to persist '{}': {e}", path.display())))?;
        Ok(())
    })
    .await
    .map_err(|e| Error::message(format!("blocking persist task failed: {e}")))?
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "Edit"
    }

    fn description(&self) -> &str {
        "Exact-match string replacement in a file. Refuses to edit if \
         `old_string` is not unique in the file unless `replace_all=true`. \
         This uniqueness requirement prevents the most common class of \
         edit mistakes. Supply enough context in `old_string` to make the \
         match unique."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["file_path", "old_string", "new_string"],
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to edit."
                },
                "old_string": {
                    "type": "string",
                    "description": "Exact text to replace. Must be unique in the file unless replace_all=true."
                },
                "new_string": {
                    "type": "string",
                    "description": "Replacement text. Must differ from old_string."
                },
                "replace_all": {
                    "type": "boolean",
                    "default": false,
                    "description": "If true, replace every occurrence of old_string instead of requiring uniqueness."
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let file_path = params
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'file_path' parameter"))?;
        let old_string = params
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'old_string' parameter"))?;
        let new_string = params
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'new_string' parameter"))?;
        let replace_all = params
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);

        match self
            .edit_impl(file_path, old_string, new_string, replace_all)
            .await
        {
            Ok(value) => Ok(value),
            Err(e) => {
                #[cfg(feature = "metrics")]
                counter!(
                    tools_metrics::EXECUTION_ERRORS_TOTAL,
                    labels::TOOL => "Edit".to_string()
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

    #[test]
    fn apply_edit_unique_match() {
        let content = "hello world";
        let updated = apply_edit(content, "world", "rust", false).unwrap();
        assert_eq!(updated, "hello rust");
    }

    #[test]
    fn apply_edit_rejects_non_unique() {
        let content = "foo foo foo";
        let err = apply_edit(content, "foo", "bar", false).unwrap_err();
        assert!(err.to_string().contains("matches 3 locations"));
    }

    #[test]
    fn apply_edit_replace_all() {
        let content = "foo foo foo";
        let updated = apply_edit(content, "foo", "bar", true).unwrap();
        assert_eq!(updated, "bar bar bar");
    }

    #[test]
    fn apply_edit_rejects_empty_old_string() {
        let err = apply_edit("x", "", "y", false).unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[test]
    fn apply_edit_rejects_identical_strings() {
        let err = apply_edit("abc", "abc", "abc", false).unwrap_err();
        assert!(err.to_string().contains("must differ"));
    }

    #[test]
    fn apply_edit_rejects_no_match() {
        let err = apply_edit("hello", "world", "rust", false).unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn edit_tool_updates_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        tokio::fs::write(&target, "alpha beta gamma").await.unwrap();

        let tool = EditTool::new();
        let value = tool
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "beta",
                "new_string": "BETA",
            }))
            .await
            .unwrap();

        assert_eq!(value["replacements"], 1);
        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "alpha BETA gamma");
    }

    #[tokio::test]
    async fn edit_tool_rejects_non_unique_match() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        tokio::fs::write(&target, "foo foo").await.unwrap();

        let tool = EditTool::new();
        let err = tool
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "foo",
                "new_string": "bar",
            }))
            .await
            .unwrap_err();

        assert!(err.to_string().contains("matches 2 locations"));
        // File must be unchanged.
        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "foo foo");
    }

    #[tokio::test]
    async fn edit_tool_replace_all() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        tokio::fs::write(&target, "foo foo foo").await.unwrap();

        let tool = EditTool::new();
        let value = tool
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "foo",
                "new_string": "bar",
                "replace_all": true,
            }))
            .await
            .unwrap();

        assert_eq!(value["replacements"], 3);
        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "bar bar bar");
    }

    #[tokio::test]
    async fn edit_tool_rejects_symlink() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real.txt");
        tokio::fs::write(&real, "foo").await.unwrap();
        let link = dir.path().join("link.txt");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        #[cfg(not(unix))]
        return;

        let tool = EditTool::new();
        let err = tool
            .execute(json!({
                "file_path": link.to_str().unwrap(),
                "old_string": "foo",
                "new_string": "bar",
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("symbolic link"));
    }
}
