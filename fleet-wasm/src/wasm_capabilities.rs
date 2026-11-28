use crate::{
    WasmAgents, WasmBlobs, WasmCredentials, WasmError, WasmOrigin, WasmRoles, WasmToolCalls,
    WasmWorkspaceAccess,
};
use core::{fmt, mem};
use std::{path::PathBuf, sync::Arc};
use wasmtime_wasi::{FsPerms, WasiCtx, filesystem::WasiFilesystemCtx};

/// Access granted to a WebAssembly provider.
///
/// Defaults to no access. Components may import every host interface regardless.
/// Fleet interface calls without the matching grant are denied, HTTP requests are
/// denied unless an origin is listed, and only mounted directories are visible.
#[derive(Clone, Debug, Default)]
pub struct WasmCapabilities {
    /// Blobs available to model and tool calls.
    pub blobs: Option<WasmBlobs>,
    /// Whether private workers may outlive individual provider calls.
    pub workers: bool,
    /// Origins that outgoing HTTP requests may use.
    pub http: Vec<WasmOrigin>,
    /// Directories available to the provider.
    pub filesystem: Vec<WasmMount>,
    /// Whether programs can run with host permissions outside the WASI sandbox.
    pub process: bool,
    /// Credentials available to this provider.
    pub credentials: Option<WasmCredentials>,
    /// Roles available to tool calls.
    pub roles: Option<WasmRoles>,
    /// Role-driven workspaces available to tool calls.
    pub workspace_access: Option<WasmWorkspaceAccess>,
    /// Agents available within tool calls and workspace resolution.
    pub agents: Option<WasmAgents>,
    /// Access to the calling agent's own tool calls, available within tool calls.
    pub tool_calls: Option<WasmToolCalls>,
}

/// A Fleet capability that calls are denied without.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WasmCapability {
    Blobs,
    Workers,
    Process,
    Credentials,
    Roles,
    WorkspaceAccess,
    Agents,
    ToolCalls,
}

/// A host directory exposed to a WebAssembly provider.
#[derive(Clone, Debug)]
pub struct WasmMount {
    /// The directory on the host.
    pub host_path: PathBuf,
    /// The path visible to the provider.
    pub guest_path: String,
    /// Whether the directory and its contents are read-only.
    pub read_only: bool,
}

impl WasmCapabilities {
    pub(crate) fn granted(&self) -> Arc<[WasmCapability]> {
        WasmCapability::ALL
            .into_iter()
            .filter(|capability| self.grants(*capability))
            .collect()
    }

    const fn grants(&self, capability: WasmCapability) -> bool {
        match capability {
            WasmCapability::Blobs => self.blobs.is_some(),
            WasmCapability::Workers => self.workers,
            WasmCapability::Process => self.process,
            WasmCapability::Credentials => self.credentials.is_some(),
            WasmCapability::Roles => self.roles.is_some(),
            WasmCapability::WorkspaceAccess => self.workspace_access.is_some(),
            WasmCapability::Agents => self.agents.is_some(),
            WasmCapability::ToolCalls => self.tool_calls.is_some(),
        }
    }

    pub(crate) fn open_filesystem(&self) -> Result<WasiFilesystemCtx, WasmError> {
        let mut builder = WasiCtx::builder();
        for mount in &self.filesystem {
            let permissions = if mount.read_only {
                FsPerms::ReadOnly
            } else {
                FsPerms::ReadWrite
            };
            builder
                .preopened_dir(&mount.host_path, &mount.guest_path, permissions)
                .map_err(|error| {
                    WasmError::runtime(error.context(format!(
                        "failed to mount {} at {}",
                        mount.host_path.display(),
                        mount.guest_path,
                    )))
                })?;
        }

        Ok(mem::take(builder.build().filesystem()))
    }
}

impl WasmCapability {
    const ALL: [Self; 8] = [
        Self::Blobs,
        Self::Workers,
        Self::Process,
        Self::Credentials,
        Self::Roles,
        Self::WorkspaceAccess,
        Self::Agents,
        Self::ToolCalls,
    ];

    pub(crate) fn denial(self) -> String {
        format!("this provider was not granted {self}")
    }
}

impl fmt::Display for WasmCapability {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Blobs => "blobs",
            Self::Workers => "workers",
            Self::Process => "process",
            Self::Credentials => "credentials",
            Self::Roles => "roles",
            Self::WorkspaceAccess => "workspace access",
            Self::Agents => "agents",
            Self::ToolCalls => "tool calls",
        })
    }
}

#[cfg(test)]
mod tests {
    use crate::{
        WasmAgents, WasmCapability, WasmEnvironment, WasmState,
        bindings::{
            agents, blobs, credentials, roles, tool_calls, types, workers, workspace_access,
        },
    };
    use core::assert_matches;
    use fleet_core::{AgentId, BlobId, MessageId};
    use fleet_kit::WorkspaceId;
    use wasmtime::{
        Config, Engine, Store,
        component::{Accessor, HasSelf},
    };

    type Host = HasSelf<WasmState>;

    fn run(test: impl AsyncFnOnce(&Accessor<WasmState>)) {
        run_with(WasmEnvironment::default(), test);
    }

    fn run_with(environment: WasmEnvironment, test: impl AsyncFnOnce(&Accessor<WasmState>)) {
        let mut config = Config::new();
        config.wasm_component_model_async(true);
        let state = WasmState::new(environment, None);
        let mut store = Store::new(&Engine::new(&config).unwrap(), state);
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(store.run_concurrent(async |accessor| test(accessor).await))
            .unwrap();
    }

    fn agent() -> types::AgentId {
        AgentId::from(1).into()
    }

    fn configured() -> types::Configured {
        types::Configured {
            name: "example".into(),
            config: "{}".into(),
        }
    }

    #[test]
    fn ungranted_credentials_and_workers_are_denied() {
        run(async |accessor| {
            let errors = [
                <Host as credentials::HostWithStore<WasmState>>::get(accessor, "key".into())
                    .await
                    .map(drop),
                <Host as credentials::HostWithStore<WasmState>>::refresh(
                    accessor,
                    "key".into(),
                    "token".into(),
                )
                .await
                .map(drop),
                <Host as workers::HostWithStore<WasmState>>::connect(
                    accessor,
                    "key".into(),
                    Vec::new(),
                )
                .await
                .unwrap()
                .map(drop),
            ];

            assert_eq!(
                errors.map(Result::unwrap_err),
                [
                    "this provider was not granted credentials",
                    "this provider was not granted credentials",
                    "this provider was not granted workers",
                ]
            );
        });
    }

    #[test]
    fn ungranted_blobs_are_denied() {
        let blob = || BlobId::from(1).into();
        run(async |accessor| {
            for error in [
                <Host as blobs::HostWithStore<WasmState>>::get(accessor, blob())
                    .await
                    .map(drop),
                <Host as blobs::HostWithStore<WasmState>>::read(accessor, blob(), 1)
                    .await
                    .map(drop),
                <Host as blobs::HostWithStore<WasmState>>::put(
                    accessor,
                    blob(),
                    "text/plain".into(),
                    Vec::new(),
                )
                .await
                .map(drop),
            ] {
                let error = error.unwrap_err();
                assert_matches!(error.kind, blobs::BlobsErrorKind::Denied);
                assert_eq!(error.message, "this provider was not granted blobs");
            }
        });
    }

    #[test]
    fn ungranted_roles_are_denied() {
        let role = roles::Role {
            name: "role".into(),
            parameters: Vec::new(),
            model: configured(),
            tools: Vec::new(),
            system_prompt: String::new(),
            compaction: None,
            extensions: "{}".into(),
        };
        let version = roles::RoleVersion { high: 0, low: 1 };
        run(async |accessor| {
            for error in [
                <Host as roles::HostWithStore<WasmState>>::list(accessor)
                    .await
                    .map(drop),
                <Host as roles::HostWithStore<WasmState>>::get(accessor, "role".into())
                    .await
                    .map(drop),
                <Host as roles::HostWithStore<WasmState>>::put(accessor, "key".into(), role, None)
                    .await,
                <Host as roles::HostWithStore<WasmState>>::delete(
                    accessor,
                    "key".into(),
                    "role".into(),
                    version,
                )
                .await,
            ] {
                let error = error.unwrap_err();
                assert_matches!(error.kind, roles::RolesErrorKind::Other);
                assert_eq!(error.message, "this provider was not granted roles");
            }
        });
    }

    #[test]
    fn ungranted_agents_are_denied() {
        let query = agents::EventQuery {
            from: None,
            until: None,
            direction: agents::EventDirection::Forward,
            limit: 1,
        };
        run(async |accessor| {
            for error in [
                <Host as agents::HostWithStore<WasmState>>::get(accessor, agent())
                    .await
                    .map(drop),
                <Host as agents::HostWithStore<WasmState>>::events(accessor, agent(), query)
                    .await
                    .map(drop),
                <Host as agents::HostWithStore<WasmState>>::wait_for_turn_end(
                    accessor,
                    agent(),
                    MessageId::from(1).into(),
                )
                .await
                .map(drop),
                <Host as agents::HostWithStore<WasmState>>::create(
                    accessor,
                    "key".into(),
                    types::Configured {
                        name: "example".into(),
                        config: "[]".into(),
                    },
                    "{}".into(),
                )
                .await
                .map(drop),
                <Host as agents::HostWithStore<WasmState>>::send(
                    accessor,
                    "key".into(),
                    agent(),
                    Vec::new(),
                )
                .await
                .map(drop),
            ] {
                let error = error.unwrap_err();
                assert_matches!(error.kind, agents::AgentsErrorKind::AccessDenied);
                assert_eq!(error.message, "this provider was not granted agents");
            }
        });

        let store = fleet_store::Store::memory().unwrap();
        let environment = WasmEnvironment {
            agents: Some(WasmAgents::new(move |agent, _| store.agent_access(agent))),
            granted: [WasmCapability::Agents].into(),
            ..WasmEnvironment::default()
        };
        run_with(environment, async |accessor| {
            let error = <Host as agents::HostWithStore<WasmState>>::get(accessor, agent())
                .await
                .unwrap_err();
            assert_matches!(error.kind, agents::AgentsErrorKind::Other);
            assert_eq!(error.message, "agents are not available in this operation");
        });
    }

    #[test]
    fn ungranted_tool_calls_are_denied() {
        run(async |accessor| {
            for error in [
                <Host as tool_calls::HostWithStore<WasmState>>::state(accessor, None, Vec::new())
                    .await
                    .map(drop),
                <Host as tool_calls::HostWithStore<WasmState>>::interrupt(
                    accessor,
                    "key".into(),
                    Vec::new(),
                )
                .await,
            ] {
                let error = error.unwrap_err();
                assert_matches!(error.kind, tool_calls::ToolCallsErrorKind::Other);
                assert_eq!(error.message, "this provider was not granted tool calls");
            }
        });
    }

    #[test]
    fn ungranted_workspace_access_is_denied() {
        let binding = || workspace_access::WorkspaceBinding {
            context: types::WorkspaceContext {
                workspace: configured(),
                agent_id: agent(),
                id: WorkspaceId::from(1).into(),
            },
            directory: None,
        };
        let file = types::WorkspaceWrite {
            path: "file".into(),
            content: Vec::new(),
            expected: None,
            create_parents: false,
        };
        let command = types::WorkspaceCommand {
            id: "command".into(),
            program: "true".into(),
            args: Vec::new(),
            env: Vec::new(),
            cwd: None,
            stdin: Vec::new(),
            timeout_ms: 1000,
            output_limit: 1,
        };
        run(async |accessor| {
            for error in [
                <Host as workspace_access::HostWithStore<WasmState>>::open(accessor)
                    .await
                    .map(drop),
                <Host as workspace_access::HostWithStore<WasmState>>::read(
                    accessor,
                    binding(),
                    "file".into(),
                    1,
                )
                .await
                .map(drop),
                <Host as workspace_access::HostWithStore<WasmState>>::write(
                    accessor,
                    binding(),
                    file,
                )
                .await,
                <Host as workspace_access::HostWithStore<WasmState>>::exec(
                    accessor,
                    binding(),
                    false,
                    command,
                )
                .await
                .map(drop),
            ] {
                let error = error.unwrap_err();
                assert_matches!(error.kind, types::WorkspacesErrorKind::Other);
                assert_eq!(
                    error.message,
                    "this provider was not granted workspace access"
                );
            }
        });
    }
}
