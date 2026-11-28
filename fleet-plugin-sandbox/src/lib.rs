//! A WebAssembly plugin providing a scripted model, an in-memory workspace and a sign-in
//! connection for Fleet's sandbox.

mod authentication;
mod bindings;
mod config;
mod files;
mod models;
mod script;
mod workspaces;

pub(crate) use self::{
    bindings::{
        exports::fleet::providers::models::{ModelsError, ModelsErrorKind},
        fleet::providers::types::{
            ContentBlock, ModelCompaction, ModelInput, ModelOutput, ModelProvenance,
            WorkspacesError, WorkspacesErrorKind,
        },
    },
    config::Config,
};
use core::fmt;

/// The name of the simulated connection.
const CONNECTION: &str = "account";
/// The provider ID in response provenance.
const PROVIDER: &str = "sandbox";

/// A scripted model, an in-memory workspace and a sign-in connection.
struct Sandbox;

#[cfg(target_arch = "wasm32")]
bindings::export!(Sandbox with_types_in bindings);

fn other(error: &impl fmt::Display) -> WorkspacesError {
    WorkspacesError {
        kind: WorkspacesErrorKind::Other,
        message: error.to_string(),
    }
}
