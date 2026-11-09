use crate::{WorkspacesError, WorkspacesErrorKind, bindings};
use bindings::wasi::{
    filesystem::{
        preopens,
        types::{Descriptor, DescriptorFlags, DescriptorType, ErrorCode, OpenFlags, PathFlags},
    },
    random::random,
};
use futures_util::future;
use wit_bindgen::StreamResult;

pub struct Directory(Descriptor);

impl Directory {
    pub async fn open(path: &str) -> Result<Self, WorkspacesError> {
        let mut mounts = preopens::get_directories();
        mounts.sort_by_key(|(_, path)| core::cmp::Reverse(path.len()));
        for (descriptor, mount) in mounts {
            let relative = if path == mount {
                Some(".")
            } else if mount == "/" {
                path.strip_prefix('/')
            } else {
                path.strip_prefix(mount.trim_end_matches('/'))
                    .and_then(|path| path.strip_prefix('/'))
            };
            if let Some(relative) = relative {
                return Self(descriptor).directory(relative, false).await;
            }
        }
        Err(crate::error(
            WorkspacesErrorKind::InvalidConfiguration,
            format!("no filesystem mount covers {path}"),
        ))
    }

    pub async fn directory(&self, path: &str, create: bool) -> Result<Self, WorkspacesError> {
        let path = crate::config::relative_path(path)?;
        let flags = DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY;
        let mut directory = self
            .0
            .open_at(
                PathFlags::SYMLINK_FOLLOW,
                ".".into(),
                OpenFlags::DIRECTORY,
                flags,
            )
            .await?;
        for part in path.split('/').filter(|part| !matches!(*part, "" | ".")) {
            if create {
                match directory.create_directory_at(part.into()).await {
                    Ok(()) | Err(ErrorCode::Exist) => {}
                    Err(error) => return Err(error.into()),
                }
            }
            directory = directory
                .open_at(
                    PathFlags::SYMLINK_FOLLOW,
                    part.into(),
                    OpenFlags::DIRECTORY,
                    flags,
                )
                .await?;
        }
        Ok(Self(directory))
    }

    pub async fn read(&self, path: &str, limit: u64) -> Result<Vec<u8>, WorkspacesError> {
        let path = crate::config::relative_path(path)?;
        let file = self
            .0
            .open_at(
                PathFlags::SYMLINK_FOLLOW,
                path.into(),
                OpenFlags::empty(),
                DescriptorFlags::READ,
            )
            .await?;
        let stat = file.stat().await?;
        if !matches!(stat.type_, DescriptorType::RegularFile) {
            return Err(crate::error(
                WorkspacesErrorKind::Other,
                "path is not a regular file",
            ));
        }
        if stat.size > limit {
            return Err(crate::error(
                WorkspacesErrorKind::FileTooLarge,
                "file exceeds the read limit",
            ));
        }
        let limit = usize::try_from(limit).unwrap_or(usize::MAX);
        let (mut stream, result) = file.read_via_stream(0);
        let mut content = Vec::new();
        loop {
            let (status, bytes) = stream.read(Vec::with_capacity(16 * 1024)).await;
            if bytes.len() > limit.saturating_sub(content.len()) {
                return Err(crate::error(
                    WorkspacesErrorKind::FileTooLarge,
                    "file exceeds the read limit",
                ));
            }
            content.extend(bytes);
            if matches!(status, StreamResult::Dropped) {
                break;
            }
        }
        result.await?;
        Ok(content)
    }

    pub async fn publish(
        &self,
        path: &str,
        content: Vec<u8>,
        replace: bool,
    ) -> Result<bool, WorkspacesError> {
        let temporary = format!(
            ".fleet-{:016x}{:016x}.tmp",
            random::get_random_u64(),
            random::get_random_u64(),
        );
        let file = self
            .0
            .open_at(
                PathFlags::empty(),
                temporary.clone(),
                OpenFlags::CREATE | OpenFlags::EXCLUSIVE,
                DescriptorFlags::WRITE,
            )
            .await?;
        let result = async {
            let (mut writer, reader) = bindings::wit_stream::new();
            let result = file.write_via_stream(reader, 0);
            let (remaining, written) = future::join(
                async move {
                    let remaining = writer.write_all(content).await;
                    drop(writer);
                    remaining
                },
                async { result.await },
            )
            .await;
            written?;
            if !remaining.is_empty() {
                return Err(crate::error(
                    WorkspacesErrorKind::Other,
                    "file write ended early",
                ));
            }
            file.sync().await?;
            drop(file);
            if replace {
                self.0
                    .rename_at(temporary.clone(), &self.0, path.into())
                    .await?;
            } else {
                match self
                    .0
                    .link_at(PathFlags::empty(), temporary.clone(), &self.0, path.into())
                    .await
                {
                    Ok(()) => {}
                    Err(ErrorCode::Exist) => return Ok(false),
                    Err(error) => return Err(error.into()),
                }
            }
            Ok(true)
        }
        .await;
        let cleanup = self.0.unlink_file_at(temporary).await;
        if result.is_ok() && !matches!(cleanup, Ok(()) | Err(ErrorCode::NoEntry)) {
            cleanup?;
        }
        result
    }
}

impl From<ErrorCode> for WorkspacesError {
    fn from(error: ErrorCode) -> Self {
        let kind = match error {
            ErrorCode::NoEntry => WorkspacesErrorKind::FileNotFound,
            _ => WorkspacesErrorKind::Other,
        };
        crate::error(kind, error.to_string())
    }
}
