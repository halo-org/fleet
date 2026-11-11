use fleet_core::{JsonObject, Model};
use fleet_models::OpenAiConfig;
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Config {
    base_url: String,
    provider_id: String,
    models: Vec<ModelConfig>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ModelConfig {
    name: String,
    context_window: u32,
    compaction_threshold: u32,
    #[serde(default)]
    extensions: JsonObject,
}

impl From<Config> for OpenAiConfig {
    fn from(config: Config) -> Self {
        Self {
            base_url: config.base_url,
            provider_id: config.provider_id,
            models: config.models.into_iter().map(Into::into).collect(),
            store: false,
        }
    }
}

impl From<ModelConfig> for Model {
    fn from(config: ModelConfig) -> Self {
        Self {
            name: config.name.into(),
            context_window: config.context_window,
            compaction_threshold: config.compaction_threshold,
            extensions: config.extensions,
        }
    }
}
