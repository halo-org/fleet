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
    pub async fn open(path: &str, create: bool) -> Result<Self, WorkspacesError> {
        open(path, create, true).await.map(Self)
    }

    pub async fn read(&self, path: &str, limit: u64) -> Result<Vec<u8>, WorkspacesError> {
        let file = self
            .0
            .open_at(
                PathFlags::SYMLINK_FOLLOW,
                path.into(),
                OpenFlags::empty(),
                DescriptorFlags::READ,
            )
            .await?;
        read_file(file, limit).await
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

pub async fn read(path: &str, limit: u64) -> Result<Vec<u8>, WorkspacesError> {
    read_file(open(path, false, false).await?, limit).await
}

async fn read_file(file: Descriptor, limit: u64) -> Result<Vec<u8>, WorkspacesError> {
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

async fn open(path: &str, create: bool, directory: bool) -> Result<Descriptor, WorkspacesError> {
    let mut mounts = preopens::get_directories();
    mounts.sort_by_key(|(_, path)| core::cmp::Reverse(path.len()));
    let (descriptor, relative) = mounts
        .into_iter()
        .find_map(|(descriptor, mount)| {
            let mount = mount.trim_end_matches('/');
            let relative = if path.trim_end_matches('/') == mount {
                Some(".")
            } else {
                path.strip_prefix(mount)?.strip_prefix('/')
            }?;
            Some((descriptor, relative.to_owned()))
        })
        .ok_or_else(|| {
            crate::error(
                WorkspacesErrorKind::InvalidConfiguration,
                format!("no filesystem mount covers {path}"),
            )
        })?;

    if create {
        for end in relative
            .match_indices('/')
            .map(|(index, _)| index)
            .chain(core::iter::once(relative.len()))
        {
            let parent = relative.get(..end).expect("path boundary is valid");
            if parent.is_empty() {
                continue;
            }
            match descriptor.create_directory_at(parent.into()).await {
                Ok(()) | Err(ErrorCode::Exist) => {}
                Err(error) => return Err(error.into()),
            }
        }
    }
    let (flags, permissions) = if directory {
        (
            OpenFlags::DIRECTORY,
            DescriptorFlags::READ | DescriptorFlags::MUTATE_DIRECTORY,
        )
    } else {
        (OpenFlags::empty(), DescriptorFlags::READ)
    };
    descriptor
        .open_at(PathFlags::SYMLINK_FOLLOW, relative, flags, permissions)
        .await
        .map_err(Into::into)
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
