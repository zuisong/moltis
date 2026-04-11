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

/// Outcome of a successful [`apply_edit`] call.
///
/// Carries enough metadata for the tool response to describe how the edit
/// landed, including whether CRLF recovery fired.
#[derive(Debug, Clone)]
pub(crate) struct EditOutcome {
    pub content: String,
    pub replacements: usize,
    pub recovered_via_crlf: bool,
}

/// Apply a single exact-match edit to `content`, enforcing uniqueness semantics.
///
/// Returns an [`EditOutcome`] on success. Errors surface the number of matches
/// so the caller (and the LLM via the tool result) can tell why the edit
/// failed.
///
/// # CRLF recovery
///
/// [`ReadTool`](super::read::ReadTool) strips `\r` from CRLF files so the LLM
/// sees them as LF. If the LLM then crafts an `Edit` with a `\n`-only
/// `old_string` against a CRLF file, the literal match will miss. On zero
/// literal hits we retry with `\r\n` substituted for every `\n` in the needle
/// and `new_string`, so CRLF round-trips work transparently through the
/// Read + Edit pipeline.
pub(crate) fn apply_edit(
    content: &str,
    old_string: &str,
    new_string: &str,
    replace_all: bool,
) -> Result<EditOutcome> {
    if old_string.is_empty() {
        return Err(Error::message("'old_string' must not be empty"));
    }
    if old_string == new_string {
        return Err(Error::message("'new_string' must differ from 'old_string'"));
    }

    let match_count = content.matches(old_string).count();
    if match_count > 0 {
        return finish_edit(
            content,
            old_string,
            new_string,
            replace_all,
            match_count,
            false,
        );
    }

    // Recovery: CRLF-ify the needle and try again.
    if content.contains("\r\n") && old_string.contains('\n') && !old_string.contains("\r\n") {
        let crlf_old = old_string.replace('\n', "\r\n");
        let crlf_new = new_string.replace('\n', "\r\n");
        let crlf_count = content.matches(&crlf_old).count();
        if crlf_count > 0 {
            return finish_edit(content, &crlf_old, &crlf_new, replace_all, crlf_count, true);
        }
    }

    Err(Error::message(
        "'old_string' not found in file — edit refused",
    ))
}

fn finish_edit(
    content: &str,
    needle: &str,
    replacement: &str,
    replace_all: bool,
    match_count: usize,
    recovered_via_crlf: bool,
) -> Result<EditOutcome> {
    if match_count > 1 && !replace_all {
        return Err(Error::message(format!(
            "'old_string' matches {match_count} locations in the file; \
             refusing to edit. Supply a larger `old_string` with more \
             context to make the match unique, or set `replace_all=true`."
        )));
    }

    let updated = if replace_all {
        content.replace(needle, replacement)
    } else {
        content.replacen(needle, replacement, 1)
    };
    let replacements = if replace_all {
        match_count
    } else {
        1
    };

    Ok(EditOutcome {
        content: updated,
        replacements,
        recovered_via_crlf,
    })
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

        let outcome = apply_edit(&content, old_string, new_string, replace_all)?;

        persist_atomic(&canonical, &outcome.content).await?;

        #[cfg(feature = "metrics")]
        counter!(
            tools_metrics::EXECUTIONS_TOTAL,
            labels::TOOL => "Edit".to_string(),
            labels::SUCCESS => "true".to_string()
        )
        .increment(1);

        Ok(json!({
            "file_path": canonical.to_string_lossy(),
            "replacements": outcome.replacements,
            "replace_all": replace_all,
            "recovered_via_crlf": outcome.recovered_via_crlf,
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
        let outcome = apply_edit(content, "world", "rust", false).unwrap();
        assert_eq!(outcome.content, "hello rust");
        assert_eq!(outcome.replacements, 1);
        assert!(!outcome.recovered_via_crlf);
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
        let outcome = apply_edit(content, "foo", "bar", true).unwrap();
        assert_eq!(outcome.content, "bar bar bar");
        assert_eq!(outcome.replacements, 3);
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

    #[test]
    fn apply_edit_crlf_recovery_single_line() {
        // File is CRLF; needle is LF-only (as Read would have returned).
        let content = "one\r\ntwo\r\nthree\r\n";
        let outcome = apply_edit(content, "one\ntwo", "ONE\nTWO", false).unwrap();
        assert!(outcome.recovered_via_crlf);
        assert_eq!(outcome.replacements, 1);
        assert_eq!(outcome.content, "ONE\r\nTWO\r\nthree\r\n");
    }

    #[test]
    fn apply_edit_crlf_recovery_preserves_file_line_endings() {
        // New-string is also LF-only; recovery CRLF-ifies it so the file
        // stays CRLF throughout after the edit.
        let content = "alpha\r\nbeta\r\n";
        let outcome = apply_edit(content, "alpha\nbeta", "ALPHA\nBETA", false).unwrap();
        assert_eq!(outcome.content, "ALPHA\r\nBETA\r\n");
    }

    #[test]
    fn apply_edit_no_recovery_when_lf_file() {
        // LF file + LF needle — happy path, recovery does not fire.
        let content = "alpha\nbeta\n";
        let outcome = apply_edit(content, "alpha\nbeta", "ALPHA\nBETA", false).unwrap();
        assert!(!outcome.recovered_via_crlf);
    }

    #[test]
    fn apply_edit_no_recovery_when_needle_already_crlf() {
        // File CRLF, needle CRLF — literal match wins, no recovery fires.
        let content = "one\r\ntwo\r\n";
        let outcome = apply_edit(content, "one\r\ntwo", "ONE\r\nTWO", false).unwrap();
        assert!(!outcome.recovered_via_crlf);
    }

    #[test]
    fn apply_edit_crlf_recovery_still_fails_if_no_match() {
        // Not present in any form.
        let content = "alpha\r\nbeta\r\n";
        let err = apply_edit(content, "delta\nepsilon", "X\nY", false).unwrap_err();
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
    async fn edit_tool_recovers_crlf_file_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("crlf.txt");
        tokio::fs::write(&target, "line one\r\nline two\r\nline three\r\n")
            .await
            .unwrap();

        // LLM sends LF-only needle (matching what Read stripped).
        let tool = EditTool::new();
        let value = tool
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "old_string": "line one\nline two",
                "new_string": "LINE ONE\nLINE TWO",
            }))
            .await
            .unwrap();

        assert_eq!(value["replacements"], 1);
        assert_eq!(value["recovered_via_crlf"], true);

        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "LINE ONE\r\nLINE TWO\r\nline three\r\n");
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
