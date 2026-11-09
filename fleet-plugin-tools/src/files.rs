use crate::{
    Config, Error, Tool, ToolOutput, WorkspaceContext, WorkspacesError, WorkspacesErrorKind,
    workspaces,
};
use serde::Deserialize;
use serde_json::json;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Read {
    path: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Write {
    path: String,
    content: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditFile {
    path: String,
    edits: Vec<Edit>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Edit {
    old: String,
    new: String,
}

pub fn metadata(name: &str) -> Option<Tool> {
    Some(match name {
        "read" => crate::metadata(
            "read",
            "Read",
            "Reads a UTF-8 file in the workspace. Files exceeding the configured read limit are rejected. Use Bash to read sections of larger files.",
            &json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "description": "File path. Relative paths start in the workspace directory.",
                    },
                },
                "required": ["path"],
                "additionalProperties": false,
            }),
        ),
        "write" => crate::metadata(
            "write",
            "Write",
            "Creates or completely replaces a UTF-8 file in the workspace. Creates missing parent directories.",
            &json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "description": "File path. Relative paths start in the workspace directory.",
                    },
                    "content": {
                        "type": "string",
                        "description": "Complete file contents.",
                    },
                },
                "required": ["path", "content"],
                "additionalProperties": false,
            }),
        ),
        "edit" => crate::metadata(
            "edit",
            "Edit",
            "Applies exact text replacements in order to one UTF-8 file. Each old text must occur exactly once at that step. The file is unchanged if any replacement fails.",
            &json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "minLength": 1,
                        "description": "File path. Relative paths start in the workspace directory.",
                    },
                    "edits": {
                        "type": "array",
                        "minItems": 1,
                        "items": {
                            "type": "object",
                            "properties": {
                                "old": {
                                    "type": "string",
                                    "minLength": 1,
                                },
                                "new": {
                                    "type": "string",
                                },
                            },
                            "required": ["old", "new"],
                            "additionalProperties": false,
                        },
                    },
                },
                "required": ["path", "edits"],
                "additionalProperties": false,
            }),
        ),
        _ => return None,
    })
}

pub async fn read(
    config: &Config,
    workspace: WorkspaceContext,
    input: &str,
) -> Result<ToolOutput, Error> {
    let input: Read = serde_json::from_str(input)?;
    validate_path(&input.path)?;
    let bytes =
        workspaces::read(workspace, input.path, config.read_limit.unwrap_or(65_536)).await?;
    let text = String::from_utf8(bytes)
        .map_err(|_| Error::Input("The file is not valid UTF-8.".into()))?;
    Ok(crate::success_output(text))
}

pub async fn write(
    workspace: WorkspaceContext,
    is_retry: bool,
    input: &str,
) -> Result<ToolOutput, Error> {
    let input: Write = serde_json::from_str(input)?;
    validate_path(&input.path)?;
    if is_retry {
        return Err(unsafe_retry());
    }
    let length = input.content.len();
    workspaces::write(
        workspace,
        workspaces::WorkspaceWrite {
            path: input.path.clone(),
            content: input.content.into_bytes(),
            expected: None,
            create_parents: true,
        },
    )
    .await?;
    Ok(crate::success_output(format!(
        "Wrote {length} bytes to {}.",
        input.path
    )))
}

pub async fn edit(
    config: &Config,
    workspace: WorkspaceContext,
    is_retry: bool,
    input: &str,
) -> Result<ToolOutput, Error> {
    let input: EditFile = serde_json::from_str(input)?;
    validate_path(&input.path)?;
    if is_retry {
        return Err(unsafe_retry());
    }
    let original = workspaces::read(
        workspace.clone(),
        input.path.clone(),
        config.read_limit.unwrap_or(65_536),
    )
    .await?;
    let text = core::str::from_utf8(&original)
        .map_err(|_| Error::Input("The file is not valid UTF-8.".into()))?;
    let updated = apply_edits(text, &input.edits)?;
    workspaces::write(
        workspace,
        workspaces::WorkspaceWrite {
            path: input.path.clone(),
            content: updated.into_bytes(),
            expected: Some(original),
            create_parents: false,
        },
    )
    .await?;
    Ok(crate::success_output(format!("Edited {}.", input.path)))
}

fn apply_edits(original: &str, edits: &[Edit]) -> Result<String, Error> {
    if edits.is_empty() {
        return Err(Error::Input("At least one edit is required.".into()));
    }
    let mut updated = original.to_owned();
    for (index, edit) in edits.iter().enumerate() {
        let number = index.checked_add(1).expect("edit number fits in usize");
        if edit.old.is_empty() || edit.old == edit.new {
            return Err(Error::Input(format!(
                "Edit {number} needs nonempty old text and different new text."
            )));
        }
        let Some(start) = updated.find(&edit.old) else {
            return Err(Error::Input(format!(
                "Edit {number} did not match. The file was not changed."
            )));
        };
        if updated.rfind(&edit.old) != Some(start) {
            return Err(Error::Input(format!(
                "Edit {number} matched more than once. The file was not changed."
            )));
        }
        let end = start
            .checked_add(edit.old.len())
            .expect("match is within the file");
        updated.replace_range(start..end, &edit.new);
    }
    Ok(updated)
}

pub fn validate_path(path: &str) -> Result<(), Error> {
    if path.is_empty() || path.contains('\0') {
        return Err(Error::Input(
            "Paths must not be empty or contain null bytes.".into(),
        ));
    }
    Ok(())
}

fn unsafe_retry() -> Error {
    Error::Workspace(WorkspacesError {
        kind: WorkspacesErrorKind::UnsafeToRetry,
        message: "The file may have already changed. Read it before trying again.".into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_path_accepts_absolute_and_parent_paths() {
        for path in ["../file", "dir/../file", "/file", "C:\\project\\file"] {
            validate_path(path).unwrap();
        }
    }

    #[test]
    fn apply_edits_uses_previous_replacements() {
        let edits = [
            Edit {
                old: "hello".into(),
                new: "héllo".into(),
            },
            Edit {
                old: "héllo world".into(),
                new: "done".into(),
            },
        ];

        let text = apply_edits("hello world", &edits).unwrap();

        assert_eq!(text, "done");
    }

    #[test]
    fn apply_edits_rejects_missing_and_ambiguous_matches() {
        for original in ["missing", "aa aa", "aaa"] {
            let edits = [Edit {
                old: "aa".into(),
                new: "b".into(),
            }];

            let error = apply_edits(original, &edits).unwrap_err();

            assert!(matches!(error, Error::Input(_)));
        }
    }
}
