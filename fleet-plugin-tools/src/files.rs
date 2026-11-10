use crate::{
    AgentId, BlobId, Config, ContentBlock, Error, Tool, ToolContext, ToolOutput, WorkspaceContext,
    WorkspacesError, WorkspacesErrorKind, blobs, workspaces,
};
use serde::Deserialize;
use serde_json::json;
use sha2::{Digest as _, Sha256};

const IMAGE_LIMIT: u64 = 10 * 1024 * 1024;

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
            "Reads a UTF-8 file or a PNG, JPEG, GIF or WebP image in the workspace. Text files exceeding the configured read limit are rejected. Use Bash to read sections of larger files.",
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
    context: &ToolContext,
    config: &Config,
    workspace: WorkspaceContext,
    input: &str,
) -> Result<ToolOutput, Error> {
    let input: Read = serde_json::from_str(input)?;
    validate_path(&input.path)?;
    let limit = config.read_limit.unwrap_or(65_536);
    let bytes = workspaces::read(workspace, input.path.clone(), limit.max(IMAGE_LIMIT)).await?;
    if let Some(media_type) = image_type(&bytes) {
        let id = blob_id(context.agent_id, media_type, &bytes);
        let info = blobs::put(id, media_type.into(), bytes).await?;
        let description = match info.image {
            Some(image) => format!(
                "{}: {media_type} image, {}x{} pixels.",
                input.path, image.width, image.height
            ),
            None => format!("{}: {media_type} image.", input.path),
        };
        return Ok(ToolOutput {
            content: vec![ContentBlock::Text(description), ContentBlock::Image(id)],
            structured_content: None,
            is_error: false,
        });
    }
    if u64::try_from(bytes.len()).map_or(true, |length| length > limit) {
        return Err(Error::Input(
            "The file exceeds the read limit. Use Bash to read sections of larger files.".into(),
        ));
    }
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

fn image_type(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        Some("image/gif")
    } else if bytes.starts_with(b"RIFF") && bytes.get(8..12) == Some(b"WEBP") {
        Some("image/webp")
    } else {
        None
    }
}

fn blob_id(agent_id: AgentId, media_type: &str, content: &[u8]) -> BlobId {
    let digest = Sha256::new()
        .chain_update(agent_id.high.to_be_bytes())
        .chain_update(agent_id.low.to_be_bytes())
        .chain_update(media_type)
        .chain_update([0])
        .chain_update(content)
        .finalize();
    let id = u128::from_be_bytes(*digest.first_chunk().expect("digest has 16 bytes"));
    BlobId {
        high: u64::try_from(id >> 64).expect("high bits fit in u64"),
        low: u64::try_from(id & u128::from(u64::MAX)).expect("low bits fit in u64"),
    }
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
    fn image_type_detects_supported_signatures() {
        for (bytes, media_type) in [
            (b"\x89PNG\r\n\x1a\n".as_slice(), Some("image/png")),
            (b"\xff\xd8\xff\xe0", Some("image/jpeg")),
            (b"GIF89a", Some("image/gif")),
            (b"RIFF\0\0\0\0WEBP", Some("image/webp")),
            (b"RIFF\0\0\0\0WAVE", None),
            (b"<svg></svg>", None),
        ] {
            assert_eq!(image_type(bytes), media_type);
        }
    }

    #[test]
    fn blob_id_is_stable_within_an_agent() {
        let id = |low, media_type| {
            let id = blob_id(AgentId { high: 1, low }, media_type, b"image");
            (id.high, id.low)
        };

        assert_eq!(id(2, "image/png"), id(2, "image/png"));
        assert_ne!(id(2, "image/png"), id(3, "image/png"));
        assert_ne!(id(2, "image/png"), id(2, "image/gif"));
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
