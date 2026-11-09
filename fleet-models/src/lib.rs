//! Model providers for [`fleet_core`].
//!
//! Providers accept an HTTP client and credentials. The `reqwest` feature adds
//! a native HTTP client. No async runtime is required when using a custom client.
//!
//! ```no_run
//! # #[cfg(feature = "reqwest")]
//! # fn example(metadata: Vec<fleet_core::Model>, key: &str) -> Result<(), Box<dyn core::error::Error>> {
//! use fleet_models::{ApiKey, OpenAiConfig, OpenAiModels, ReqwestHttp};
//!
//! let models = OpenAiModels::new(
//!     ReqwestHttp::new()?,
//!     ApiKey::new(key)?,
//!     OpenAiConfig {
//!         models: metadata,
//!         ..OpenAiConfig::default()
//!     },
//! )?;
//! # Ok(())
//! # }
//! ```

mod credentials;
mod http;
mod openai;
mod openai_error;

#[cfg(feature = "reqwest")]
mod reqwest_http;

pub use self::{
    credentials::{ApiKey, Credentials},
    http::Http,
    openai::{OpenAiConfig, OpenAiModels},
    openai_error::OpenAiError,
};

#[cfg(feature = "reqwest")]
pub use self::reqwest_http::ReqwestHttp;
