use crate::{
    Sandbox, WorkspacesError,
    bindings::{
        self,
        exports::fleet::providers::worker::Guest,
        fleet::host::workers::{self, Connection, Listener},
    },
};
use futures_util::future;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use wit_bindgen::{StreamReader, StreamResult};

const WORKER: &str = "files";

/// A request to the worker holding workspace files.
#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Request {
    Read {
        workspace: String,
        path: String,
        limit: u64,
    },
    Write {
        workspace: String,
        path: String,
        content: Vec<u8>,
        expected: Option<Vec<u8>>,
    },
    Release {
        workspace: String,
    },
}

/// The worker's reply to a request.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Reply {
    Contents(Vec<u8>),
    Written,
    Released,
    NotFound,
    TooLarge,
    Changed,
}

/// The files of every workspace, keyed by workspace and path.
#[derive(Default)]
struct Files(BTreeMap<(String, String), Vec<u8>>);

impl Files {
    fn handle(&mut self, request: Request) -> Reply {
        match request {
            Request::Read {
                workspace,
                path,
                limit,
            } => match self.0.get(&(workspace, path)) {
                None => Reply::NotFound,
                Some(contents) if u64::try_from(contents.len()).unwrap_or(u64::MAX) > limit => {
                    Reply::TooLarge
                }
                Some(contents) => Reply::Contents(contents.clone()),
            },
            Request::Write {
                workspace,
                path,
                content,
                expected,
            } => {
                let key = (workspace, path);
                if let Some(expected) = expected
                    && self.0.get(&key) != Some(&expected)
                {
                    return Reply::Changed;
                }
                self.0.insert(key, content);
                Reply::Written
            }
            Request::Release { workspace } => {
                self.0.retain(|(owner, _), _| *owner != workspace);
                Reply::Released
            }
        }
    }
}

impl Guest for Sandbox {
    async fn run(_initialization: Vec<u8>, incoming: Listener) -> Result<(), String> {
        let mut files = Files::default();
        while let Some(connection) = incoming.accept().await {
            drop(serve(connection, &mut files).await);
        }
        Ok(())
    }
}

/// Sends a request to the worker holding workspace files.
pub async fn send(request: &Request) -> Result<Reply, WorkspacesError> {
    let bytes = serde_json::to_vec(request).map_err(|error| crate::other(&error))?;
    let connection = workers::connect(WORKER.into(), Vec::new())
        .await
        .map_err(|error| crate::other(&error))?;
    let output = connection.read().map_err(|error| crate::other(&error))?;
    let (mut writer, reader) = bindings::wit_stream::new();
    let written = connection
        .write(reader)
        .map_err(|error| crate::other(&error))?;
    let ((), reply) = future::try_join(
        async move {
            let remaining = writer.write_all(bytes).await;
            drop(writer);
            if !remaining.is_empty() {
                return Err("the files worker closed the connection".into());
            }
            written.await
        },
        async move {
            let bytes = read(output.data).await;
            output.result.await?;
            Ok(bytes)
        },
    )
    .await
    .map_err(|error: String| crate::other(&error))?;
    serde_json::from_slice(&reply).map_err(|error| crate::other(&error))
}

async fn serve(connection: Connection, files: &mut Files) -> Result<(), String> {
    let input = connection.read()?;
    let request = read(input.data).await;
    input.result.await?;
    let request = serde_json::from_slice(&request).map_err(|error| error.to_string())?;
    let reply = files.handle(request);
    let bytes = serde_json::to_vec(&reply).map_err(|error| error.to_string())?;
    let (mut writer, reader) = bindings::wit_stream::new();
    let written = connection.write(reader)?;
    let remaining = writer.write_all(bytes).await;
    drop(writer);
    if !remaining.is_empty() {
        return Err("the client closed the connection".into());
    }
    written.await
}

async fn read(mut data: StreamReader<u8>) -> Vec<u8> {
    let mut bytes = Vec::new();
    loop {
        let (status, chunk) = data.read(Vec::with_capacity(16 * 1024)).await;
        bytes.extend(chunk);
        if matches!(status, StreamResult::Dropped) {
            return bytes;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_file(path: &str, content: &str, expected: Option<&str>) -> Request {
        Request::Write {
            workspace: "a".into(),
            path: path.into(),
            content: content.into(),
            expected: expected.map(Into::into),
        }
    }

    fn read_file(workspace: &str, path: &str, limit: u64) -> Request {
        Request::Read {
            workspace: workspace.into(),
            path: path.into(),
            limit,
        }
    }

    #[test]
    fn files_are_kept_per_workspace() {
        let mut files = Files::default();

        assert_eq!(
            files.handle(write_file("/notes", "hello", None)),
            Reply::Written
        );
        assert_eq!(
            files.handle(read_file("a", "/notes", 5)),
            Reply::Contents(b"hello".to_vec())
        );
        assert_eq!(files.handle(read_file("a", "/notes", 4)), Reply::TooLarge);
        assert_eq!(files.handle(read_file("b", "/notes", 5)), Reply::NotFound);
    }

    #[test]
    fn release_drops_only_its_workspace() {
        let mut files = Files::default();
        files.handle(write_file("/notes", "hello", None));
        files.handle(Request::Write {
            workspace: "b".into(),
            path: "/notes".into(),
            content: b"other".to_vec(),
            expected: None,
        });

        let release = || Request::Release {
            workspace: "a".into(),
        };
        assert_eq!(files.handle(release()), Reply::Released);
        assert_eq!(files.handle(release()), Reply::Released);

        assert_eq!(files.handle(read_file("a", "/notes", 5)), Reply::NotFound);
        assert_eq!(
            files.handle(read_file("b", "/notes", 5)),
            Reply::Contents(b"other".to_vec())
        );
    }

    #[test]
    fn writes_check_expected_contents() {
        let mut files = Files::default();

        assert_eq!(
            files.handle(write_file("/notes", "new", Some("old"))),
            Reply::Changed
        );
        files.handle(write_file("/notes", "old", None));
        assert_eq!(
            files.handle(write_file("/notes", "new", Some("other"))),
            Reply::Changed
        );
        assert_eq!(
            files.handle(write_file("/notes", "new", Some("old"))),
            Reply::Written
        );
        assert_eq!(
            files.handle(read_file("a", "/notes", 3)),
            Reply::Contents(b"new".to_vec())
        );
    }
}
