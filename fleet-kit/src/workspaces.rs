use alloc::{boxed::Box, collections::BTreeMap, rc::Rc, string::String, sync::Arc, vec::Vec};
use core::fmt;
use fleet_core::{Configured, JsonObject};

// === Workspaces ===

/// The metadata of a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Workspace {
    /// The name that uniquely identifies this workspace.
    pub name: WorkspaceName,
    /// Additional data not interpreted by kit.
    pub extensions: JsonObject,
}

/// The name of a workspace.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceName(String);

/// The ID of a workspace environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WorkspaceId(u128);

/// A workspace environment and its configuration.
///
/// Retain this value to reopen the environment or share it with another agent.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceContext {
    /// The workspace name and config.
    pub workspace: Configured<WorkspaceName>,
    /// The ID of the environment.
    pub id: WorkspaceId,
}

// === Files ===

/// A file to create or replace in a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceWrite {
    /// The file path in the workspace environment.
    pub path: String,
    /// The new contents.
    pub content: Vec<u8>,
    /// The expected contents, or `None` to replace any contents.
    pub expected: Option<Vec<u8>>,
    /// Whether to create missing parent directories.
    pub create_parents: bool,
}

// === Commands ===

/// A program to run in a workspace.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceCommand {
    /// The ID of this execution, unique within the workspace.
    pub id: String,
    /// The program to execute.
    pub program: String,
    /// The arguments passed to the program.
    pub args: Vec<String>,
    /// Environment variables to set for this execution.
    pub env: BTreeMap<String, String>,
    /// The working directory, or `None` for the workspace directory.
    pub cwd: Option<String>,
    /// The bytes sent to standard input before closing it.
    pub stdin: Vec<u8>,
    /// The maximum execution time in milliseconds.
    pub timeout_ms: u64,
    /// The maximum bytes to retain from each output stream.
    pub output_limit: u64,
}

/// The result of running a workspace command.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct WorkspaceOutput {
    /// How the program ended.
    pub exit: WorkspaceExit,
    /// The captured standard output.
    pub stdout: Vec<u8>,
    /// The captured standard error.
    pub stderr: Vec<u8>,
    /// Whether standard output exceeded the limit.
    pub stdout_truncated: bool,
    /// Whether standard error exceeded the limit.
    pub stderr_truncated: bool,
}

/// How a workspace command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspaceExit {
    /// The program exited with this code.
    Code(i32),
    /// The program was terminated without an exit code.
    Terminated,
    /// The program exceeded its time limit and was terminated.
    TimedOut,
}

// === Provider ===

/// Provides workspace environments.
///
/// Environment identity survives provider restarts. Sharing a context shares the environment.
/// Different IDs do not guarantee isolation, for example when they name the same local directory.
/// Paths use `/` separators. Relative paths start in the workspace directory.
/// Absolute paths refer to the workspace environment. Access is controlled by the provider.
pub trait Workspaces {
    /// The error type returned by this provider.
    type Error: fleet_core::Error<WorkspacesErrorKind> + 'static;

    /// Returns the workspace with the given name.
    ///
    /// # Errors
    ///
    /// Returns an error when the provider fails to perform the lookup.
    fn get(
        &self,
        name: &WorkspaceName,
    ) -> impl Future<Output = Result<Option<Workspace>, Self::Error>> + Send;

    /// Creates or reopens a workspace environment.
    ///
    /// Concurrent and repeated calls with the same context open the same environment.
    /// Cancelling a call may leave provisioning in progress. Retrying must resume it.
    /// Dropping the provider does not remove environments.
    ///
    /// # Errors
    ///
    /// Returns [`InvalidConfiguration`](WorkspacesErrorKind::InvalidConfiguration) if an existing
    /// ID is used with incompatible configuration. Returns other errors when opening fails.
    fn open(
        &self,
        context: &WorkspaceContext,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Reads a file from an open workspace.
    ///
    /// # Errors
    ///
    /// Returns [`FileTooLarge`](WorkspacesErrorKind::FileTooLarge) if the file exceeds `limit`.
    /// Returns other errors when the file cannot be read.
    fn read(
        &self,
        context: &WorkspaceContext,
        path: &str,
        limit: u64,
    ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send;

    /// Creates or replaces a file in an open workspace.
    ///
    /// Readers see either the old or new contents. Checks expected contents before replacement.
    /// This is not an atomic comparison against changes made by programs outside the provider.
    ///
    /// # Errors
    ///
    /// Returns [`FileChanged`](WorkspacesErrorKind::FileChanged) if expected contents do not match,
    /// including when the file is missing. Returns other errors when the file cannot be written.
    fn write(
        &self,
        context: &WorkspaceContext,
        file: &WorkspaceWrite,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send;

    /// Runs a command to completion in an open workspace.
    ///
    /// `is_retry` indicates that this execution may have been attempted before. An earlier attempt
    /// may still be running or may have completed. Reuse the command ID when retrying.
    /// Cancelling the call does not guarantee that the program stops.
    /// Timeouts and nonzero exit codes are returned as command outcomes.
    ///
    /// # Errors
    ///
    /// Returns [`UnsafeToRetry`](WorkspacesErrorKind::UnsafeToRetry) if a retry could repeat effects.
    /// Returns other errors when the provider cannot execute or observe the command.
    fn exec(
        &self,
        context: &WorkspaceContext,
        is_retry: bool,
        command: &WorkspaceCommand,
    ) -> impl Future<Output = Result<WorkspaceOutput, Self::Error>> + Send;
}

/// The kind of error returned by a workspace provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum WorkspacesErrorKind {
    /// The workspace or environment does not exist.
    NotFound,
    /// The workspace configuration is invalid.
    InvalidConfiguration,
    /// The file does not exist.
    FileNotFound,
    /// The file exceeds the read limit.
    FileTooLarge,
    /// The file does not match its expected contents.
    FileChanged,
    /// Retrying the command risks repeating effects of an earlier attempt.
    UnsafeToRetry,
    /// An error not covered by any other kind.
    Other,
}

// === Macros ===

macro_rules! impl_workspaces_for_pointer {
    ($t:ty $(, $lifetime:lifetime)?) => {
        #[warn(clippy::missing_trait_methods)]
        impl<$($lifetime,)? T> Workspaces for $t
        where
            T: Workspaces + ?Sized,
        {
            type Error = T::Error;

            fn get(
                &self,
                name: &WorkspaceName,
            ) -> impl Future<Output = Result<Option<Workspace>, Self::Error>> + Send {
                (**self).get(name)
            }

            fn open(
                &self,
                context: &WorkspaceContext,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).open(context)
            }

            fn read(
                &self,
                context: &WorkspaceContext,
                path: &str,
                limit: u64,
            ) -> impl Future<Output = Result<Vec<u8>, Self::Error>> + Send {
                (**self).read(context, path, limit)
            }

            fn write(
                &self,
                context: &WorkspaceContext,
                file: &WorkspaceWrite,
            ) -> impl Future<Output = Result<(), Self::Error>> + Send {
                (**self).write(context, file)
            }

            fn exec(
                &self,
                context: &WorkspaceContext,
                is_retry: bool,
                command: &WorkspaceCommand,
            ) -> impl Future<Output = Result<WorkspaceOutput, Self::Error>> + Send {
                (**self).exec(context, is_retry, command)
            }
        }
    };
}

// === Impls ===

impl_workspaces_for_pointer!(&'a T, 'a);
impl_workspaces_for_pointer!(&'a mut T, 'a);
impl_workspaces_for_pointer!(Box<T>);
impl_workspaces_for_pointer!(Arc<T>);
impl_workspaces_for_pointer!(Rc<T>);

impl From<String> for WorkspaceName {
    fn from(value: String) -> Self {
        Self(value)
    }
}

impl From<&str> for WorkspaceName {
    fn from(value: &str) -> Self {
        Self(value.into())
    }
}

impl From<WorkspaceName> for String {
    fn from(value: WorkspaceName) -> Self {
        value.0
    }
}

impl AsRef<str> for WorkspaceName {
    fn as_ref(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WorkspaceName {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl From<u128> for WorkspaceId {
    fn from(value: u128) -> Self {
        Self(value)
    }
}

impl From<WorkspaceId> for u128 {
    fn from(value: WorkspaceId) -> Self {
        value.0
    }
}

impl fmt::Display for WorkspaceId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl fmt::Display for WorkspacesErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotFound => f.write_str("workspace not found"),
            Self::InvalidConfiguration => f.write_str("invalid configuration"),
            Self::FileNotFound => f.write_str("file not found"),
            Self::FileTooLarge => f.write_str("file too large"),
            Self::FileChanged => f.write_str("file changed"),
            Self::UnsafeToRetry => f.write_str("unsafe to retry"),
            Self::Other => f.write_str("other error"),
        }
    }
}
