use crate::{
    JsonObject, JsonValue, RoleParameter, RoleParameterName, RoleParameterTarget, ToolAlias,
};
use alloc::{collections::BTreeMap, string::String, vec::Vec};
use core::fmt;

#[derive(Debug)]
pub enum ParametersError {
    OverlappingTargets {
        first: RoleParameterName,
        second: RoleParameterName,
    },
    NonObjectRoot {
        parameter: RoleParameterName,
    },
}

#[derive(Clone, Copy)]
enum Target<'a> {
    Model,
    Tool(&'a ToolAlias),
    Extensions,
}

struct Binding<'a> {
    name: &'a RoleParameterName,
    path: &'a [String],
    value: Option<&'a JsonValue>,
}

pub fn apply_model_parameters(
    config: JsonObject,
    parameters: &BTreeMap<RoleParameterName, RoleParameter>,
    values: &JsonObject,
) -> Result<JsonObject, ParametersError> {
    apply_parameters(config, parameters, values, Target::Model)
}

pub fn apply_tool_parameters(
    tool: &ToolAlias,
    config: JsonObject,
    parameters: &BTreeMap<RoleParameterName, RoleParameter>,
    values: &JsonObject,
) -> Result<JsonObject, ParametersError> {
    apply_parameters(config, parameters, values, Target::Tool(tool))
}

pub fn apply_extension_parameters(
    extensions: JsonObject,
    parameters: &BTreeMap<RoleParameterName, RoleParameter>,
    values: &JsonObject,
) -> Result<JsonObject, ParametersError> {
    apply_parameters(extensions, parameters, values, Target::Extensions)
}

fn apply_parameters(
    mut object: JsonObject,
    parameters: &BTreeMap<RoleParameterName, RoleParameter>,
    values: &JsonObject,
    target: Target<'_>,
) -> Result<JsonObject, ParametersError> {
    let mut bindings = parameters
        .iter()
        .filter_map(|(name, parameter)| {
            let path = match (target, &parameter.target) {
                (Target::Model, RoleParameterTarget::Model(path))
                | (Target::Extensions, RoleParameterTarget::Extensions(path)) => path,
                (Target::Tool(alias), RoleParameterTarget::Tool(tool, path)) if alias == tool => {
                    path
                }
                _ => return None,
            };

            Some(Binding {
                name,
                path,
                value: values.get(name.as_ref()),
            })
        })
        .collect::<Vec<_>>();

    bindings.sort_unstable_by(|a, b| a.path.cmp(b.path));

    for [first, second] in bindings.array_windows() {
        if second.path.starts_with(first.path) {
            return Err(ParametersError::OverlappingTargets {
                first: first.name.clone(),
                second: second.name.clone(),
            });
        }
    }

    for binding in bindings {
        let Some(value) = binding.value else {
            continue;
        };

        match (binding.path.split_last(), value) {
            (Some((key, parents)), value) => {
                let mut target = &mut object;

                for parent in parents {
                    let value = target
                        .entry(parent.clone())
                        .or_insert_with(|| JsonValue::Object(JsonObject::new()));

                    if !value.is_object() {
                        *value = JsonValue::Object(JsonObject::new());
                    }

                    target = value.as_object_mut().expect("value is an object");
                }

                merge(target.entry(key.clone()).or_insert(JsonValue::Null), value);
            }
            (None, JsonValue::Object(value)) => merge_object(&mut object, value),
            (None, _) => {
                return Err(ParametersError::NonObjectRoot {
                    parameter: binding.name.clone(),
                });
            }
        }
    }

    Ok(object)
}

fn merge(target: &mut JsonValue, value: &JsonValue) {
    match (target, value) {
        (JsonValue::Object(target), JsonValue::Object(value)) => merge_object(target, value),
        (target, value) => target.clone_from(value),
    }
}

fn merge_object(target: &mut JsonObject, value: &JsonObject) {
    for (key, value) in value {
        merge(target.entry(key.clone()).or_insert(JsonValue::Null), value);
    }
}

impl fmt::Display for ParametersError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OverlappingTargets { first, second } => write!(
                f,
                "parameters {first} and {second} have overlapping targets",
            ),
            Self::NonObjectRoot { parameter } => write!(
                f,
                "parameter {parameter} must be an object when targeting the root",
            ),
        }
    }
}

impl core::error::Error for ParametersError {}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;
    use core::assert_matches;
    use serde_json::json;

    fn object(value: JsonValue) -> JsonObject {
        match value {
            JsonValue::Object(object) => object,
            _ => panic!("expected an object"),
        }
    }

    #[test]
    fn apply_model_parameters_deep_merges_objects() {
        let parameters = BTreeMap::from([(
            "settings".into(),
            RoleParameter {
                target: RoleParameterTarget::Model(vec![]),
                extensions: JsonObject::new(),
            },
        )]);

        let config = object(json!({
            "options": {
                "format": "text",
                "limit": 10
            },
            "enabled": true,
        }));
        let values = object(json!({
            "settings": {
                "options": {
                    "format": "json",
                },
                "tags": ["test"],
            },
        }));
        let expected = object(json!({
            "options": {
                "format": "json",
                "limit": 10,
            },
            "enabled": true,
            "tags": ["test"],
        }));

        let result = apply_model_parameters(config, &parameters, &values).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn apply_model_parameters_replaces_non_object_values() {
        let parameters = BTreeMap::from([(
            "setting".into(),
            RoleParameter {
                target: RoleParameterTarget::Model(vec!["value".into()]),
                extensions: JsonObject::new(),
            },
        )]);

        let cases = [
            (json!(10), json!(20)),
            (json!([1, 2]), json!([3])),
            (
                json!({
                    "enabled": true,
                }),
                json!(null),
            ),
        ];

        for (previous, value) in cases {
            let config = object(json!({
                "value": previous,
            }));
            let values = object(json!({
                "setting": value,
            }));
            let expected = object(json!({
                "value": value,
            }));

            let result = apply_model_parameters(config, &parameters, &values).unwrap();
            assert_eq!(result, expected);
        }
    }

    #[test]
    fn apply_model_parameters_creates_nested_objects() {
        let parameters = BTreeMap::from([(
            "format".into(),
            RoleParameter {
                target: RoleParameterTarget::Model(vec!["options".into(), "format".into()]),
                extensions: JsonObject::new(),
            },
        )]);

        let cases = [
            JsonObject::new(),
            object(json!({
                "options": false,
            })),
        ];
        let values = object(json!({
            "format": "json",
        }));
        let expected = object(json!({
            "options": {
                "format": "json",
            },
        }));

        for config in cases {
            let result = apply_model_parameters(config, &parameters, &values).unwrap();
            assert_eq!(result, expected);
        }
    }

    #[test]
    fn apply_model_parameters_ignores_missing_and_unknown_values() {
        let parameters = BTreeMap::from([(
            "limit".into(),
            RoleParameter {
                target: RoleParameterTarget::Model(vec!["limit".into()]),
                extensions: JsonObject::new(),
            },
        )]);

        let config = object(json!({
            "limit": 10,
        }));
        let values = object(json!({
            "unknown": 20,
        }));
        let expected = config.clone();

        let result = apply_model_parameters(config, &parameters, &values).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn apply_extension_parameters_deep_merges_objects() {
        let parameters = BTreeMap::from([(
            "workspace".into(),
            RoleParameter {
                target: RoleParameterTarget::Extensions(vec!["workspace".into()]),
                extensions: JsonObject::new(),
            },
        )]);

        let extensions = object(json!({
            "workspace": {
                "machine": "development",
                "config": {
                    "directory": "/repo",
                    "read_only": false,
                },
            },
        }));
        let values = object(json!({
            "workspace": {
                "config": {
                    "directory": "/project",
                },
            },
        }));
        let expected = object(json!({
            "workspace": {
                "machine": "development",
                "config": {
                    "directory": "/project",
                    "read_only": false,
                },
            },
        }));

        let result = apply_extension_parameters(extensions, &parameters, &values).unwrap();
        assert_eq!(result, expected);
    }

    #[test]
    fn apply_parameters_uses_matching_targets() {
        let parameters = BTreeMap::from([
            (
                "model_limit".into(),
                RoleParameter {
                    target: RoleParameterTarget::Model(vec!["limit".into()]),
                    extensions: JsonObject::new(),
                },
            ),
            (
                "search_limit".into(),
                RoleParameter {
                    target: RoleParameterTarget::Tool("search".into(), vec!["limit".into()]),
                    extensions: JsonObject::new(),
                },
            ),
            (
                "fetch_limit".into(),
                RoleParameter {
                    target: RoleParameterTarget::Tool("fetch".into(), vec!["limit".into()]),
                    extensions: JsonObject::new(),
                },
            ),
            (
                "extension_limit".into(),
                RoleParameter {
                    target: RoleParameterTarget::Extensions(vec!["limit".into()]),
                    extensions: JsonObject::new(),
                },
            ),
        ]);

        let config = JsonObject::new();
        let values = object(json!({
            "model_limit": 10,
            "search_limit": 20,
            "fetch_limit": 30,
            "extension_limit": 40,
        }));

        let result = apply_model_parameters(config.clone(), &parameters, &values).unwrap();
        assert_eq!(
            result,
            object(json!({
                "limit": 10,
            })),
        );

        let result =
            apply_tool_parameters(&"search".into(), config.clone(), &parameters, &values).unwrap();
        assert_eq!(
            result,
            object(json!({
                "limit": 20,
            })),
        );

        let result =
            apply_tool_parameters(&"fetch".into(), config.clone(), &parameters, &values).unwrap();
        assert_eq!(
            result,
            object(json!({
                "limit": 30,
            })),
        );

        let result = apply_extension_parameters(config, &parameters, &values).unwrap();
        assert_eq!(
            result,
            object(json!({
                "limit": 40,
            })),
        );
    }

    #[test]
    fn apply_model_parameters_rejects_overlapping_targets() {
        let paths = [
            vec![],
            vec!["options".into()],
            vec!["options".into(), "limit".into()],
        ];

        for path in paths {
            let parameters = BTreeMap::from([
                (
                    "setting_a".into(),
                    RoleParameter {
                        target: RoleParameterTarget::Model(vec!["options".into()]),
                        extensions: JsonObject::new(),
                    },
                ),
                (
                    "setting_b".into(),
                    RoleParameter {
                        target: RoleParameterTarget::Model(path),
                        extensions: JsonObject::new(),
                    },
                ),
            ]);

            let config = JsonObject::new();
            let values = JsonObject::new();

            let error = apply_model_parameters(config, &parameters, &values).unwrap_err();
            assert_matches!(
                error,
                ParametersError::OverlappingTargets { first, second } if matches!(
                    (first.as_ref(), second.as_ref()),
                    ("setting_a", "setting_b") | ("setting_b", "setting_a")
                ),
            );
        }
    }

    #[test]
    fn apply_model_parameters_rejects_non_object_root() {
        let parameters = BTreeMap::from([(
            "settings".into(),
            RoleParameter {
                target: RoleParameterTarget::Model(vec![]),
                extensions: JsonObject::new(),
            },
        )]);

        let config = JsonObject::new();
        let values = object(json!({
            "settings": 42,
        }));

        let error = apply_model_parameters(config, &parameters, &values).unwrap_err();
        assert_matches!(
            error,
            ParametersError::NonObjectRoot { parameter } if parameter.as_ref() == "settings",
        );
    }
}
