//! `MultiEdit` tool — atomic batch of sequential edits to a single file.
//!
//! All edits succeed or none apply. Each edit runs against the buffer
//! produced by the previous edit, so later edits see the output of earlier
//! ones. This is the right semantic for multi-step file surgery: the LLM
//! plans the whole sequence, and we guarantee rollback on any failure.

use {
    async_trait::async_trait,
    moltis_agents::tool_registry::AgentTool,
    serde_json::{Value, json},
    tracing::instrument,
};

#[cfg(feature = "metrics")]
use moltis_metrics::{counter, labels, tools as tools_metrics};

use crate::{
    Result,
    error::Error,
    fs::{
        edit::{apply_edit, persist_atomic},
        shared::{canonicalize_existing, ensure_regular_file, reject_if_symlink},
    },
};

/// Native `MultiEdit` tool implementation.
#[derive(Default)]
pub struct MultiEditTool;

impl MultiEditTool {
    #[must_use]
    pub fn new() -> Self {
        Self
    }

    #[instrument(skip(self, edits), fields(file_path = %file_path, edit_count = edits.len()))]
    async fn multi_edit_impl(&self, file_path: &str, edits: Vec<ParsedEdit>) -> Result<Value> {
        if edits.is_empty() {
            return Err(Error::message("'edits' must contain at least one edit"));
        }

        reject_if_symlink(file_path).await?;
        let canonical = canonicalize_existing(file_path).await?;
        ensure_regular_file(&canonical).await?;

        let original = tokio::fs::read_to_string(&canonical)
            .await
            .map_err(|e| Error::message(format!("failed to read '{file_path}': {e}")))?;

        let mut buffer = original;
        let mut per_edit_replacements: Vec<usize> = Vec::with_capacity(edits.len());

        for (idx, edit) in edits.iter().enumerate() {
            let count = buffer.matches(&edit.old_string).count();
            let updated = apply_edit(
                &buffer,
                &edit.old_string,
                &edit.new_string,
                edit.replace_all,
            )
            .map_err(|e| Error::message(format!("edit #{}: {e}", idx + 1)))?;
            let applied = if edit.replace_all {
                count
            } else {
                1
            };
            per_edit_replacements.push(applied);
            buffer = updated;
        }

        persist_atomic(&canonical, &buffer).await?;

        #[cfg(feature = "metrics")]
        counter!(
            tools_metrics::EXECUTIONS_TOTAL,
            labels::TOOL => "MultiEdit".to_string(),
            labels::SUCCESS => "true".to_string()
        )
        .increment(1);

        Ok(json!({
            "file_path": canonical.to_string_lossy(),
            "edits_applied": edits.len(),
            "replacements_per_edit": per_edit_replacements,
        }))
    }
}

#[derive(Debug, Clone)]
struct ParsedEdit {
    old_string: String,
    new_string: String,
    replace_all: bool,
}

fn parse_edits(raw: &Value) -> Result<Vec<ParsedEdit>> {
    let arr = raw
        .as_array()
        .ok_or_else(|| Error::message("'edits' must be an array"))?;
    let mut out = Vec::with_capacity(arr.len());
    for (idx, entry) in arr.iter().enumerate() {
        let old_string = entry
            .get("old_string")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message(format!("edit #{}: missing 'old_string'", idx + 1)))?
            .to_string();
        let new_string = entry
            .get("new_string")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message(format!("edit #{}: missing 'new_string'", idx + 1)))?
            .to_string();
        let replace_all = entry
            .get("replace_all")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        out.push(ParsedEdit {
            old_string,
            new_string,
            replace_all,
        });
    }
    Ok(out)
}

#[async_trait]
impl AgentTool for MultiEditTool {
    fn name(&self) -> &str {
        "MultiEdit"
    }

    fn description(&self) -> &str {
        "Apply multiple sequential edits to a single file as one atomic \
         operation. Each edit sees the output of previous edits. Either all \
         edits succeed or none are applied (full rollback on any failure). \
         Each edit follows the same uniqueness rules as `Edit`."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["file_path", "edits"],
            "properties": {
                "file_path": {
                    "type": "string",
                    "description": "Absolute path to the file to edit."
                },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "description": "Ordered list of edits to apply atomically.",
                    "items": {
                        "type": "object",
                        "required": ["old_string", "new_string"],
                        "properties": {
                            "old_string": { "type": "string" },
                            "new_string": { "type": "string" },
                            "replace_all": { "type": "boolean", "default": false }
                        }
                    }
                }
            }
        })
    }

    async fn execute(&self, params: Value) -> anyhow::Result<Value> {
        let file_path = params
            .get("file_path")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::message("missing 'file_path' parameter"))?;
        let edits_raw = params
            .get("edits")
            .ok_or_else(|| Error::message("missing 'edits' parameter"))?;
        let edits = parse_edits(edits_raw)?;

        match self.multi_edit_impl(file_path, edits).await {
            Ok(value) => Ok(value),
            Err(e) => {
                #[cfg(feature = "metrics")]
                counter!(
                    tools_metrics::EXECUTION_ERRORS_TOTAL,
                    labels::TOOL => "MultiEdit".to_string()
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
    async fn multi_edit_applies_sequential_edits() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        tokio::fs::write(&target, "alpha beta gamma").await.unwrap();

        let tool = MultiEditTool::new();
        tool.execute(json!({
            "file_path": target.to_str().unwrap(),
            "edits": [
                { "old_string": "alpha", "new_string": "ALPHA" },
                { "old_string": "gamma", "new_string": "GAMMA" }
            ]
        }))
        .await
        .unwrap();

        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "ALPHA beta GAMMA");
    }

    #[tokio::test]
    async fn multi_edit_second_sees_first_output() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        tokio::fs::write(&target, "one").await.unwrap();

        let tool = MultiEditTool::new();
        tool.execute(json!({
            "file_path": target.to_str().unwrap(),
            "edits": [
                { "old_string": "one", "new_string": "two" },
                { "old_string": "two", "new_string": "three" }
            ]
        }))
        .await
        .unwrap();

        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "three");
    }

    #[tokio::test]
    async fn multi_edit_rolls_back_on_any_failure() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        tokio::fs::write(&target, "alpha beta").await.unwrap();

        let tool = MultiEditTool::new();
        let err = tool
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "edits": [
                    { "old_string": "alpha", "new_string": "ALPHA" },
                    { "old_string": "nope", "new_string": "NOPE" }
                ]
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("edit #2"));

        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "alpha beta");
    }

    #[tokio::test]
    async fn multi_edit_empty_array_errors() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        tokio::fs::write(&target, "alpha").await.unwrap();

        let tool = MultiEditTool::new();
        let err = tool
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "edits": []
            }))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("at least one edit"));
    }

    #[tokio::test]
    async fn multi_edit_replace_all_per_edit() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("a.txt");
        tokio::fs::write(&target, "foo foo foo bar").await.unwrap();

        let tool = MultiEditTool::new();
        let value = tool
            .execute(json!({
                "file_path": target.to_str().unwrap(),
                "edits": [
                    { "old_string": "foo", "new_string": "FOO", "replace_all": true },
                    { "old_string": "bar", "new_string": "BAR" }
                ]
            }))
            .await
            .unwrap();

        assert_eq!(value["replacements_per_edit"], json!([3, 1]));
        let contents = tokio::fs::read_to_string(&target).await.unwrap();
        assert_eq!(contents, "FOO FOO FOO BAR");
    }
}
