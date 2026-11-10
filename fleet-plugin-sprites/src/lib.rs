//! A WebAssembly plugin providing workspaces on Sprites.

mod authentication;
mod bindings;
mod config;
mod exec;
mod sprites;
mod workspaces;

pub(crate) use self::{
    bindings::exports::fleet::providers::workspaces::{
        Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit, WorkspaceId, WorkspaceOutput,
        WorkspaceWrite, WorkspacesError, WorkspacesErrorKind,
    },
    config::Config,
    sprites::Sprite,
};

/// Workspaces and authentication for Sprites.
struct Sprites;

#[cfg(target_arch = "wasm32")]
bindings::export!(Sprites with_types_in bindings);

fn error(kind: WorkspacesErrorKind, message: impl Into<String>) -> WorkspacesError {
    WorkspacesError {
        kind,
        message: message.into(),
    }
}
