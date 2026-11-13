use crate::Role;
use serde::Deserialize;
use serde_json::{Map, Value, json};

/// An exact replacement in the system prompt.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TextEdit {
    old: String,
    new: String,
}

/// Returns the JSON Schema of the `edit_role` input.
pub fn schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "The role name.",
            },
            "version": {
                "type": "string",
                "description": "The version from read_role.",
            },
            "remove": {
                "type": "array",
                "description": "JSON Pointers to fields to delete, applied first, such as \
                    /tools/bash or /model/config/effort.",
                "items": { "type": "string" },
            },
            "merge": {
                "type": "object",
                "description": "Fields merged into the role after the removals, in the shape \
                    read_role returns. Objects merge recursively. Other values replace the \
                    current value. null is stored as a value, so use remove to delete fields.",
            },
            "system_prompt_edits": {
                "type": "array",
                "description": "Exact replacements in the system prompt, applied last in order. \
                    Each old text must match exactly once.",
                "items": {
                    "type": "object",
                    "properties": {
                        "old": { "type": "string" },
                        "new": { "type": "string" },
                    },
                    "required": ["old", "new"],
                    "additionalProperties": false,
                },
            },
        },
        "required": ["name"],
        "additionalProperties": false,
    })
}

/// Applies removals, a merge and then system prompt edits to a role.
pub fn apply(
    role: &Role,
    remove: &[String],
    merge: &Map<String, Value>,
    system_prompt_edits: &[TextEdit],
) -> Result<Role, String> {
    if remove.is_empty() && merge.is_empty() && system_prompt_edits.is_empty() {
        return Err("At least one edit is required.".into());
    }
    let mut document = serde_json::to_value(role).expect("roles can be serialized");
    for path in remove {
        remove_field(&mut document, path)?;
    }
    merge_into(document.as_object_mut().expect("roles are objects"), merge);
    if !system_prompt_edits.is_empty() {
        let Some(Value::String(system_prompt)) = document.get_mut("system_prompt") else {
            return Err("The system prompt must be a string.".into());
        };
        *system_prompt = apply_text_edits(system_prompt, system_prompt_edits)?;
    }
    let edited: Role =
        serde_json::from_value(document).map_err(|error| format!("Invalid role: {error}"))?;
    if edited.name() != role.name() {
        return Err("edit_role cannot rename a role.".into());
    }
    Ok(edited)
}

fn remove_field(document: &mut Value, path: &str) -> Result<(), String> {
    let (parent_path, key) = path
        .rsplit_once('/')
        .filter(|(parent_path, _)| parent_path.is_empty() || parent_path.starts_with('/'))
        .ok_or_else(|| format!("{path} is not a JSON Pointer to a role field."))?;
    let key = key.replace("~1", "/").replace("~0", "~");
    document
        .pointer_mut(parent_path)
        .and_then(Value::as_object_mut)
        .and_then(|object| object.remove(&key))
        .map(drop)
        .ok_or_else(|| format!("{path} does not exist."))
}

fn merge_into(target: &mut Map<String, Value>, merge: &Map<String, Value>) {
    for (key, value) in merge {
        match (target.get_mut(key), value) {
            (Some(Value::Object(target)), Value::Object(merge)) => merge_into(target, merge),
            _ => {
                target.insert(key.clone(), value.clone());
            }
        }
    }
}

fn apply_text_edits(original: &str, edits: &[TextEdit]) -> Result<String, String> {
    let mut updated = original.to_owned();
    for (index, edit) in edits.iter().enumerate() {
        let number = index.checked_add(1).expect("edit number fits in usize");
        if edit.old.is_empty() || edit.old == edit.new {
            return Err(format!(
                "System prompt edit {number} needs nonempty old text and different new text."
            ));
        }
        let Some(start) = updated.find(&edit.old) else {
            return Err(format!("System prompt edit {number} did not match."));
        };
        if updated.rfind(&edit.old) != Some(start) {
            return Err(format!(
                "System prompt edit {number} matched more than once."
            ));
        }
        let end = start
            .checked_add(edit.old.len())
            .expect("match is within the system prompt");
        updated.replace_range(start..end, &edit.new);
    }
    Ok(updated)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn edit(input: Value) -> Result<Value, String> {
        let role = serde_json::from_value(json!({
            "name": "reviewer",
            "model": {
                "name": "openai:gpt",
                "config": {
                    "effort": "high",
                },
            },
            "tools": {
                "bash": {
                    "name": "tools:bash",
                    "config": {
                        "timeout_ms": 1000,
                    },
                },
            },
            "system_prompt": "Review code. Be brief.",
        }))
        .unwrap();
        let input: crate::EditInput = serde_json::from_value(input).unwrap();
        let edited = apply(
            &role,
            &input.remove,
            &input.merge,
            &input.system_prompt_edits,
        )?;
        Ok(serde_json::to_value(edited).unwrap())
    }

    #[test]
    fn edits_apply_in_order() {
        let edited = edit(json!({
            "name": "reviewer",
            "remove": ["/tools/bash"],
            "merge": {
                "model": {
                    "config": {
                        "effort": null,
                    },
                },
                "tools": {
                    "bash": {
                        "name": "tools:shell",
                    },
                },
                "system_prompt": "Review code. Be kind.",
            },
            "system_prompt_edits": [
                { "old": "kind", "new": "thorough" },
            ],
        }))
        .unwrap();

        assert_eq!(
            edited,
            json!({
                "name": "reviewer",
                "model": {
                    "name": "openai:gpt",
                    "config": {
                        "effort": null,
                    },
                },
                "tools": {
                    "bash": {
                        "name": "tools:shell",
                        "config": {},
                    },
                },
                "system_prompt": "Review code. Be thorough.",
                "parameters": {},
                "extensions": {},
            })
        );
    }

    #[test]
    fn system_prompt_edits_must_match_once() {
        let missing = edit(json!({
            "name": "reviewer",
            "system_prompt_edits": [{ "old": "Test", "new": "Run" }],
        }))
        .unwrap_err();
        let ambiguous = edit(json!({
            "name": "reviewer",
            "system_prompt_edits": [{ "old": "e", "new": "E" }],
        }))
        .unwrap_err();

        assert_eq!(missing, "System prompt edit 1 did not match.");
        assert_eq!(ambiguous, "System prompt edit 1 matched more than once.");
    }

    #[test]
    fn invalid_edits_are_rejected() {
        let missing = edit(json!({
            "name": "reviewer",
            "remove": ["/tools/read"],
        }))
        .unwrap_err();
        let rename = edit(json!({
            "name": "reviewer",
            "merge": { "name": "writer" },
        }))
        .unwrap_err();
        let empty = edit(json!({ "name": "reviewer" })).unwrap_err();

        assert_eq!(missing, "/tools/read does not exist.");
        assert_eq!(rename, "edit_role cannot rename a role.");
        assert_eq!(empty, "At least one edit is required.");
    }
}
