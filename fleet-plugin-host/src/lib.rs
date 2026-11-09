//! A WebAssembly plugin providing workspaces on the host machine.

mod bindings;
mod config;
mod filesystem;
mod process;
mod workspaces;

use self::workspaces::Host;

pub(crate) use self::{
    bindings::exports::fleet::providers::workspaces::{
        Workspace, WorkspaceCommand, WorkspaceContext, WorkspaceExit, WorkspaceOutput,
        WorkspaceWrite, WorkspacesError, WorkspacesErrorKind,
    },
    config::Config,
    filesystem::Directory,
};

bindings::export!(Host with_types_in bindings);

fn error(kind: WorkspacesErrorKind, message: impl Into<String>) -> WorkspacesError {
    WorkspacesError {
        kind,
        message: message.into(),
    }
}
