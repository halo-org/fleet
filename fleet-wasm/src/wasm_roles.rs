use crate::{
    WasmState,
    bindings::{roles, types},
};
use core::fmt;
use fleet_core::{AgentId, Configured, Role, RoleCompaction, RoleParameter, RoleParameterTarget};
use fleet_kit::{RoleStore, RoleStoreErrorKind, RoleVersion, StoredRole};
use futures_util::future::BoxFuture;
use std::{collections::BTreeMap, sync::Arc};
use wasmtime::component::{Accessor, HasSelf};

/// Roles available to WebAssembly tool calls.
///
/// Each tool call receives role access from the agent factory.
#[derive(Clone)]
pub struct WasmRoles(Arc<dyn Fn(AgentId) -> WasmAgentRoles + Send + Sync>);

/// Role access for a single agent's tool call.
#[derive(Clone)]
pub struct WasmAgentRoles(Arc<dyn RolesObject>);

trait RolesObject: Send + Sync {
    fn list(&self) -> BoxFuture<'_, Result<Vec<roles::StoredRole>, roles::RolesError>>;
    fn get(
        &self,
        name: String,
    ) -> BoxFuture<'_, Result<Option<roles::StoredRole>, roles::RolesError>>;
    fn put(
        &self,
        idempotency_key: String,
        role: roles::Role,
        expected: Option<roles::RoleVersion>,
    ) -> BoxFuture<'_, Result<(), roles::RolesError>>;
}

impl WasmRoles {
    /// Supplies a factory for agent-scoped role access.
    pub fn new<R: RoleStore + Send + Sync + 'static>(
        factory: impl Fn(AgentId) -> R + Send + Sync + 'static,
    ) -> Self {
        Self(Arc::new(move |id| WasmAgentRoles(Arc::new(factory(id)))))
    }

    pub(crate) fn for_agent(&self, id: AgentId) -> WasmAgentRoles {
        (self.0)(id)
    }
}

impl fmt::Debug for WasmRoles {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("WasmRoles").finish_non_exhaustive()
    }
}

impl<R: RoleStore + Send + Sync> RolesObject for R {
    fn list(&self) -> BoxFuture<'_, Result<Vec<roles::StoredRole>, roles::RolesError>> {
        Box::pin(async move {
            Ok(RoleStore::roles(self)
                .await
                .map_err(|source| error(&source))?
                .into_iter()
                .map(Into::into)
                .collect())
        })
    }

    fn get(
        &self,
        name: String,
    ) -> BoxFuture<'_, Result<Option<roles::StoredRole>, roles::RolesError>> {
        Box::pin(async move {
            Ok(RoleStore::role(self, &name.into())
                .await
                .map_err(|source| error(&source))?
                .map(Into::into))
        })
    }

    fn put(
        &self,
        idempotency_key: String,
        role: roles::Role,
        expected: Option<roles::RoleVersion>,
    ) -> BoxFuture<'_, Result<(), roles::RolesError>> {
        Box::pin(async move {
            let role = Role::try_from(role)?;
            RoleStore::put_role(self, &idempotency_key, &role, expected.map(Into::into))
                .await
                .map_err(|source| error(&source))
        })
    }
}

impl roles::Host for WasmState {}

impl roles::HostWithStore<WasmState> for HasSelf<WasmState> {
    async fn list(
        store: &Accessor<WasmState, Self>,
    ) -> Result<Vec<roles::StoredRole>, roles::RolesError> {
        host(store)?.0.list().await
    }

    async fn get(
        store: &Accessor<WasmState, Self>,
        name: String,
    ) -> Result<Option<roles::StoredRole>, roles::RolesError> {
        host(store)?.0.get(name).await
    }

    async fn put(
        store: &Accessor<WasmState, Self>,
        idempotency_key: String,
        role: roles::Role,
        expected: Option<roles::RoleVersion>,
    ) -> Result<(), roles::RolesError> {
        host(store)?.0.put(idempotency_key, role, expected).await
    }
}

fn host(
    store: &Accessor<WasmState, HasSelf<WasmState>>,
) -> Result<WasmAgentRoles, roles::RolesError> {
    store
        .with(|mut access| access.get().roles())
        .ok_or_else(|| roles::RolesError {
            kind: roles::RolesErrorKind::Other,
            message: "roles are not available".into(),
        })
}

impl From<StoredRole> for roles::StoredRole {
    fn from(value: StoredRole) -> Self {
        Self {
            role: value.role.into(),
            version: value.version.into(),
        }
    }
}

impl From<Role> for roles::Role {
    fn from(value: Role) -> Self {
        Self {
            name: value.name.into(),
            parameters: value
                .parameters
                .into_iter()
                .map(|(name, parameter)| roles::RoleParameter {
                    name: name.into(),
                    target: match parameter.target {
                        RoleParameterTarget::Model(path) => roles::RoleParameterTarget::Model(path),
                        RoleParameterTarget::Tool(alias, path) => {
                            roles::RoleParameterTarget::Tool((alias.into(), path))
                        }
                        RoleParameterTarget::Extensions(path) => {
                            roles::RoleParameterTarget::Extensions(path)
                        }
                    },
                    extensions: crate::convert::json(&parameter.extensions),
                })
                .collect(),
            model: configured(value.model),
            tools: value
                .tools
                .into_iter()
                .map(|(alias, tool)| roles::RoleTool {
                    alias: alias.into(),
                    tool: configured(tool),
                })
                .collect(),
            system_prompt: value.system_prompt,
            compaction: value.compaction.map(|compaction| roles::RoleCompaction {
                threshold: compaction.threshold,
                instructions: compaction.instructions,
            }),
            extensions: crate::convert::json(&value.extensions),
        }
    }
}

impl TryFrom<roles::Role> for Role {
    type Error = roles::RolesError;

    fn try_from(value: roles::Role) -> Result<Self, Self::Error> {
        let mut parameters = BTreeMap::new();
        for parameter in value.parameters {
            let target = match parameter.target {
                roles::RoleParameterTarget::Model(path) => RoleParameterTarget::Model(path),
                roles::RoleParameterTarget::Tool((alias, path)) => {
                    RoleParameterTarget::Tool(alias.into(), path)
                }
                roles::RoleParameterTarget::Extensions(path) => {
                    RoleParameterTarget::Extensions(path)
                }
            };
            let extensions = object(&parameter.extensions)?;
            if parameters
                .insert(
                    parameter.name.clone().into(),
                    RoleParameter { target, extensions },
                )
                .is_some()
            {
                return Err(invalid(format!(
                    "duplicate parameter name {}",
                    parameter.name
                )));
            }
        }
        let mut tools = BTreeMap::new();
        for tool in value.tools {
            if tools
                .insert(tool.alias.clone().into(), configured_from(tool.tool)?)
                .is_some()
            {
                return Err(invalid(format!("duplicate tool alias {}", tool.alias)));
            }
        }
        Ok(Self {
            name: value.name.into(),
            parameters,
            model: configured_from(value.model)?,
            tools,
            system_prompt: value.system_prompt,
            compaction: value.compaction.map(|compaction| RoleCompaction {
                threshold: compaction.threshold,
                instructions: compaction.instructions,
            }),
            extensions: object(&value.extensions)?,
        })
    }
}

impl From<RoleVersion> for roles::RoleVersion {
    fn from(version: RoleVersion) -> Self {
        let version = u128::from(version);
        Self {
            high: u64::try_from(version >> 64).expect("high bits fit in u64"),
            low: u64::try_from(version & u128::from(u64::MAX)).expect("low bits fit in u64"),
        }
    }
}

impl From<roles::RoleVersion> for RoleVersion {
    fn from(version: roles::RoleVersion) -> Self {
        ((u128::from(version.high) << 64) | u128::from(version.low)).into()
    }
}

fn configured<N: Into<String>>(value: Configured<N>) -> types::Configured {
    types::Configured {
        name: value.name.into(),
        config: crate::convert::json(&value.config),
    }
}

fn configured_from<N: From<String>>(
    value: types::Configured,
) -> Result<Configured<N>, roles::RolesError> {
    Ok(Configured {
        name: value.name.into(),
        config: object(&value.config)?,
    })
}

fn object(value: &str) -> Result<fleet_core::JsonObject, roles::RolesError> {
    serde_json::from_str(value).map_err(|source| invalid(source.to_string()))
}

const fn invalid(message: String) -> roles::RolesError {
    roles::RolesError {
        kind: roles::RolesErrorKind::InvalidRole,
        message,
    }
}

fn error(error: &impl fleet_core::Error<RoleStoreErrorKind>) -> roles::RolesError {
    let mut message = error.to_string();
    let mut source = error.source();
    while let Some(error) = source {
        message.push_str(": ");
        message.push_str(&error.to_string());
        source = error.source();
    }
    roles::RolesError {
        kind: match error.kind() {
            RoleStoreErrorKind::PreconditionFailed => roles::RolesErrorKind::PreconditionFailed,
            RoleStoreErrorKind::IdempotencyConflict => roles::RolesErrorKind::IdempotencyConflict,
            RoleStoreErrorKind::InvalidRole => roles::RolesErrorKind::InvalidRole,
            RoleStoreErrorKind::Other => roles::RolesErrorKind::Other,
        },
        message,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::WasmEnvironment;
    use core::assert_matches;
    use fleet_core::JsonObject;
    use serde_json::json;
    use wasmtime::{Config, Engine, Store};

    fn role() -> Role {
        let json = |value: serde_json::Value| value.as_object().unwrap().clone();
        Role {
            name: "reviewer".into(),
            parameters: BTreeMap::from([(
                "effort".into(),
                RoleParameter {
                    target: RoleParameterTarget::Tool("shell".into(), vec!["timeout".into()]),
                    extensions: json(json!({ "fleet.title": "Effort" })),
                },
            )]),
            model: Configured {
                name: "openai:gpt".into(),
                config: json(json!({ "effort": "high" })),
            },
            tools: BTreeMap::from([(
                "shell".into(),
                Configured {
                    name: "tools:shell".into(),
                    config: JsonObject::new(),
                },
            )]),
            system_prompt: "Review code.".into(),
            compaction: Some(RoleCompaction {
                threshold: Some(1000),
                instructions: None,
            }),
            extensions: json(json!({ "fleet.title": "Reviewer" })),
        }
    }

    #[test]
    fn host_writes_and_reads_roles() {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(async {
                let roles = Arc::new(fleet_store::Store::memory().unwrap());
                let mut state = WasmState::new(WasmEnvironment::default(), None);
                state.set_roles(Some(
                    WasmRoles::new(move |_| roles.clone()).for_agent(1.into()),
                ));
                let mut config = Config::new();
                config.wasm_component_model_async(true);
                let mut store = Store::new(&Engine::new(&config).unwrap(), state);

                store
                    .run_concurrent(async |accessor| {
                        <HasSelf<WasmState> as roles::HostWithStore<WasmState>>::put(
                            accessor,
                            "key".into(),
                            role().into(),
                            None,
                        )
                        .await
                        .unwrap();
                        let stored = <HasSelf<WasmState> as roles::HostWithStore<WasmState>>::get(
                            accessor,
                            "reviewer".into(),
                        )
                        .await
                        .unwrap()
                        .unwrap();
                        assert_eq!(Role::try_from(stored.role).unwrap(), role());

                        let error = <HasSelf<WasmState> as roles::HostWithStore<WasmState>>::put(
                            accessor,
                            "other".into(),
                            role().into(),
                            None,
                        )
                        .await
                        .unwrap_err();
                        assert_matches!(error.kind, roles::RolesErrorKind::PreconditionFailed);
                    })
                    .await
                    .unwrap();
            });
    }

    #[test]
    fn duplicate_tool_aliases_are_invalid() {
        let mut role = roles::Role::from(role());
        role.tools.push(role.tools[0].clone());

        let error = Role::try_from(role).unwrap_err();

        assert_matches!(error.kind, roles::RolesErrorKind::InvalidRole);
        assert_eq!(error.message, "duplicate tool alias shell");
    }
}
