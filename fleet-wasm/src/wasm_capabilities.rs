use crate::{WasmBlobs, WasmCredentials, WasmError, WasmWorkspaceHost};
use core::mem;
use std::path::PathBuf;
use wasmtime_wasi::{FsPerms, WasiCtx, filesystem::WasiFilesystemCtx};

/// Access granted to a WebAssembly provider.
///
/// Defaults to no access.
#[derive(Clone, Debug, Default)]
pub struct WasmCapabilities {
    /// Blobs available to model and tool calls.
    pub blobs: Option<WasmBlobs>,
    /// Whether outgoing HTTP requests are allowed.
    pub http: bool,
    /// Directories available to the provider.
    pub filesystem: Vec<WasmMount>,
    /// Whether programs can run with host permissions outside the WASI sandbox.
    pub process: bool,
    /// Credentials available to this provider.
    pub credentials: Option<WasmCredentials>,
    /// Workspaces available to this provider.
    pub workspaces: Option<WasmWorkspaceHost>,
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
    pub(crate) fn open_filesystem(self) -> Result<WasiFilesystemCtx, WasmError> {
        let mut builder = WasiCtx::builder();
        for mount in self.filesystem {
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
