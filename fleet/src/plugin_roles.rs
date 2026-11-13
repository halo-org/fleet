use crate::{Fleet, FleetError, RoleDefinition};
use core::{error::Error, fmt};
use fleet_core::{AgentId, Role, RoleName};
use fleet_kit::{RoleStore, RoleStoreErrorKind, RoleVersion, StoredRole};

/// Roles available to a plugin provider's tool calls for an agent.
///
/// Writes must be editable through the role API. Idempotency keys are scoped to the provider
/// and agent.
pub struct PluginRoles {
    fleet: Fleet,
    scope: String,
}

/// An error returned by [`PluginRoles`].
#[derive(Debug)]
pub enum PluginRolesError {
    /// The role name is invalid.
    InvalidName,
    /// The role cannot be edited through the role API.
    Unsupported(serde_json::Error),
    /// A fleet operation failed.
    Fleet(FleetError),
}

impl PluginRoles {
    /// Creates role access for a provider's tool calls on behalf of an agent.
    pub fn new(fleet: Fleet, provider: &str, agent: AgentId) -> Self {
        Self {
            fleet,
            scope: format!("plugin:{provider}:{agent}"),
        }
    }
}

impl RoleStore for PluginRoles {
    type Error = PluginRolesError;

    async fn roles(&self) -> Result<Vec<StoredRole>, PluginRolesError> {
        self.fleet.roles().await.map_err(PluginRolesError::Fleet)
    }

    async fn role(&self, name: &RoleName) -> Result<Option<StoredRole>, PluginRolesError> {
        self.fleet.role(name).await.map_err(PluginRolesError::Fleet)
    }

    async fn put_role(
        &self,
        idempotency_key: &str,
        role: &Role,
        expected: Option<RoleVersion>,
    ) -> Result<(), PluginRolesError> {
        if !crate::roles::valid_name(role.name.as_ref()) {
            return Err(PluginRolesError::InvalidName);
        }
        RoleDefinition::try_from(role.clone()).map_err(PluginRolesError::Unsupported)?;
        self.fleet
            .put_role(
                format!("{}:{idempotency_key}", self.scope),
                role.clone(),
                expected,
            )
            .await
            .map_err(PluginRolesError::Fleet)
    }
}

impl fmt::Display for PluginRolesError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidName => {
                f.write_str("role names may contain only ASCII letters, digits, - and _")
            }
            Self::Unsupported(_) => f.write_str("role cannot be edited through the role API"),
            Self::Fleet(_) => f.write_str("role operation failed"),
        }
    }
}

impl Error for PluginRolesError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidName => None,
            Self::Unsupported(source) => Some(source),
            Self::Fleet(source) => Some(source),
        }
    }
}

impl fleet_core::Error<RoleStoreErrorKind> for PluginRolesError {
    fn kind(&self) -> RoleStoreErrorKind {
        match self {
            Self::InvalidName | Self::Unsupported(_) => RoleStoreErrorKind::InvalidRole,
            Self::Fleet(FleetError::Store(source)) => {
                fleet_core::Error::<RoleStoreErrorKind>::kind(source)
            }
            Self::Fleet(_) => RoleStoreErrorKind::Other,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::assert_matches;
    use fleet_core::{Configured, JsonObject};
    use fleet_store::Store;

    fn role(name: &str) -> Role {
        Role {
            name: name.into(),
            parameters: [].into(),
            model: Configured {
                name: "work:model".into(),
                config: JsonObject::new(),
            },
            tools: [].into(),
            system_prompt: "Be helpful.".into(),
            compaction: None,
            extensions: JsonObject::new(),
        }
    }

    #[test]
    fn writes_are_validated_and_scoped() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let fleet = Fleet::new(Store::memory().unwrap(), None);
        let first = PluginRoles::new(fleet.clone(), "roles", 1.into());
        let second = PluginRoles::new(fleet, "roles", 2.into());
        let mut unsupported = role("custom");
        unsupported.extensions.insert("custom".into(), true.into());

        runtime.block_on(async {
            first
                .put_role("key", &role("assistant"), None)
                .await
                .unwrap();
            second
                .put_role("key", &role("reviewer"), None)
                .await
                .unwrap();
            let error = first
                .put_role("name", &role("bad name"), None)
                .await
                .unwrap_err();
            assert_matches!(error, PluginRolesError::InvalidName);
            let error = first
                .put_role("custom", &unsupported, None)
                .await
                .unwrap_err();
            assert_matches!(error, PluginRolesError::Unsupported(_));

            let roles = first.roles().await.unwrap();
            let names: Vec<_> = roles
                .iter()
                .map(|stored| stored.role.name.as_ref())
                .collect();
            assert_eq!(names, ["assistant", "reviewer"]);
        });
    }
}
