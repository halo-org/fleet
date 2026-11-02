use crate::{Probability, RandomError, Scheduler, WrapConfig, content, content::ContentConfig};
use core::fmt;
use fastrand::Rng;
use fleet_core::{Configured, JsonObject, ModelName, Role, RoleName, Roles, ToolName};
use std::collections::BTreeMap;

/// A simulated [`Roles`] provider.
pub struct RolesSimulator<'sim> {
    scheduler: &'sim Scheduler,
    config: RolesSimulatorConfig,
    roles: BTreeMap<RoleName, Role>,
}

/// An injected error returned by [`RolesSimulator`].
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct RolesSimulatorError {
    inner: RandomError,
}

struct RolesSimulatorConfig {
    get: WrapConfig,
    role_count: usize,
    include_tool: Probability,
    prompt: ContentConfig,
}

impl<'sim> RolesSimulator<'sim> {
    /// Creates a role simulator from the given seed and available models and tools.
    ///
    /// Each role selects a model and a subset of tools from the supplied names. Roles use `model-0`
    /// if no models are supplied.
    pub fn new(
        scheduler: &'sim Scheduler,
        seed: u64,
        available_models: &[ModelName],
        available_tools: &[ToolName],
    ) -> Self {
        let mut rng = Rng::with_seed(seed);
        let config = RolesSimulatorConfig::generate(&mut rng);
        let roles = generate_roles(&mut rng, &config, available_models, available_tools);

        Self {
            scheduler,
            config,
            roles,
        }
    }

    /// Returns the names of the generated roles in sorted order.
    #[must_use]
    pub fn role_names(&self) -> Vec<RoleName> {
        self.roles.keys().cloned().collect()
    }
}

impl Roles for RolesSimulator<'_> {
    type Error = RolesSimulatorError;

    async fn get(&self, name: &RoleName) -> Result<Option<Role>, Self::Error> {
        self.scheduler
            .wrap(&self.config.get, RolesSimulatorError::random, async {
                Ok(self.roles.get(name).cloned())
            })
            .await
    }
}

impl RolesSimulatorError {
    const fn random() -> Self {
        Self { inner: RandomError }
    }
}

impl fmt::Display for RolesSimulatorError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("roles simulator error")
    }
}

impl core::error::Error for RolesSimulatorError {
    fn source(&self) -> Option<&(dyn core::error::Error + 'static)> {
        Some(&self.inner)
    }
}

impl RolesSimulatorConfig {
    fn generate(rng: &mut Rng) -> Self {
        Self {
            get: WrapConfig::generate(rng),
            role_count: rng.usize(1..=5),
            include_tool: Probability::generate(rng),
            prompt: ContentConfig::generate(rng, 6),
        }
    }
}

fn generate_roles(
    rng: &mut Rng,
    config: &RolesSimulatorConfig,
    available_models: &[ModelName],
    available_tools: &[ToolName],
) -> BTreeMap<RoleName, Role> {
    (0..config.role_count)
        .map(|index| {
            let model = rng
                .choice(available_models)
                .cloned()
                .unwrap_or_else(|| "model-0".into());

            let mut tools = BTreeMap::new();

            for (tool_index, name) in available_tools.iter().enumerate() {
                if config.include_tool.sample(rng) {
                    tools.insert(
                        format!("alias-{tool_index}").into(),
                        Configured {
                            name: name.clone(),
                            config: JsonObject::new(),
                        },
                    );
                }
            }

            let role = Role {
                name: format!("role-{index}").into(),
                parameters: BTreeMap::new(),
                model: Configured {
                    name: model,
                    config: JsonObject::new(),
                },
                tools,
                system_prompt: content::generate_text(rng, &config.prompt),
                compaction: None,
                extensions: JsonObject::new(),
            };

            (role.name.clone(), role)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn role_count_and_tool_inclusion_follow_config() {
        for include in [Probability::NEVER, Probability::ALWAYS] {
            let mut rng = Rng::with_seed(0);
            let mut config = RolesSimulatorConfig::generate(&mut rng);
            config.role_count = 3;
            config.include_tool = include;
            let models = [ModelName::from("model")];
            let tools = [ToolName::from("first"), ToolName::from("second")];

            let roles = generate_roles(&mut rng, &config, &models, &tools);

            assert_eq!(roles.len(), 3);
            for role in roles.values() {
                assert_eq!(role.model.name, models[0]);
                assert_eq!(
                    role.tools.len(),
                    if include == Probability::ALWAYS { 2 } else { 0 }
                );
                assert!(role.tools.values().all(|tool| tools.contains(&tool.name)));
            }
        }
    }
}
