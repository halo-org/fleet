use crate::{WorkspacesError, WorkspacesErrorKind, bindings, config::HOME};
use bindings::{
    fleet::providers::credentials::{self, Credential},
    wasi::http::{
        client,
        types::{ErrorCode, Fields, Method, Request as HttpRequest, RequestOptions, Scheme},
    },
};
use futures_util::future;
use percent_encoding::{AsciiSet, NON_ALPHANUMERIC};
use wit_bindgen::{FutureReader, StreamReader, StreamResult};

const AUTHORITY: &str = "api.sprites.dev";
const CONNECTION: &str = "sprites";
const QUERY: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');
const SECOND: u64 = 1_000_000_000;

/// A sprite hosting workspaces.
pub struct Sprite {
    name: String,
}

struct Request {
    method: Method,
    path: String,
    content_type: Option<&'static str>,
    body: Vec<u8>,
    timeouts: Option<Timeouts>,
}

struct Timeouts {
    first_byte: u64,
    between_bytes: u64,
}

struct Response {
    status: u16,
    body: StreamReader<u8>,
    trailers: FutureReader<Result<Option<Fields>, ErrorCode>>,
}

impl Sprite {
    pub const fn new(name: String) -> Self {
        Self { name }
    }

    /// Creates the sprite unless it already exists.
    pub async fn create(&self) -> Result<(), WorkspacesError> {
        let body = serde_json::json!({
            "name": self.name,
        });
        let request = Request::new(Method::Post, "/v1/sprites".into())
            .with_body("application/json", body.to_string().into_bytes());
        let response = send(&request).await?;
        if matches!(response.status, 200 | 201) {
            return Ok(());
        }
        let error = response
            .failure(&format!("create sprite {}", self.name))
            .await;
        if self.exists().await? {
            Ok(())
        } else {
            Err(error)
        }
    }

    pub async fn read(&self, path: &str, limit: u64) -> Result<Vec<u8>, WorkspacesError> {
        let query = query(&[("path", path), ("workingDir", HOME)]);
        let request = Request::new(Method::Get, format!("{}/fs/read?{query}", self.path()));
        let response = send(&request).await?;
        match response.status {
            200 => {
                let limit = usize::try_from(limit).unwrap_or(usize::MAX);
                let bytes = response.read(limit).await?;
                if bytes.len() > limit {
                    return Err(crate::error(
                        WorkspacesErrorKind::FileTooLarge,
                        format!("{path} exceeds the read limit"),
                    ));
                }
                Ok(bytes)
            }
            404 => Err(self.missing(WorkspacesErrorKind::FileNotFound, path).await),
            _ => Err(response.failure(&format!("read {path}")).await),
        }
    }

    pub async fn write(
        &self,
        path: &str,
        content: Vec<u8>,
        create_parents: bool,
    ) -> Result<(), WorkspacesError> {
        let mkdir = if create_parents { "true" } else { "false" };
        let query = query(&[("path", path), ("workingDir", HOME), ("mkdir", mkdir)]);
        let request = Request::new(Method::Put, format!("{}/fs/write?{query}", self.path()))
            .with_body("application/octet-stream", content);
        let response = send(&request).await?;
        match response.status {
            200 => Ok(()),
            404 => Err(self.missing(WorkspacesErrorKind::FileNotFound, path).await),
            _ => Err(response.failure(&format!("write {path}")).await),
        }
    }

    pub async fn rename(&self, source: &str, dest: &str) -> Result<(), WorkspacesError> {
        let body = serde_json::json!({
            "source": source,
            "dest": dest,
            "workingDir": HOME,
            "asRoot": false,
        });
        let request = Request::new(Method::Post, format!("{}/fs/rename", self.path()))
            .with_body("application/json", body.to_string().into_bytes());
        let response = send(&request).await?;
        match response.status {
            200 => Ok(()),
            404 => Err(self
                .missing(WorkspacesErrorKind::FileNotFound, source)
                .await),
            _ => Err(response.failure(&format!("rename {source}")).await),
        }
    }

    /// Runs the command wrapper and returns its multiplexed output.
    pub async fn exec(&self, input: Vec<u8>, timeout_ms: u64) -> Result<Vec<u8>, WorkspacesError> {
        let path = format!("{}/exec?{}", self.path(), crate::exec::query());
        let mut request =
            Request::new(Method::Post, path).with_body("application/octet-stream", input);
        request.timeouts = Some(Timeouts {
            first_byte: timeout_ms
                .saturating_mul(1_000_000)
                .saturating_add(60 * SECOND),
            between_bytes: 60 * SECOND,
        });
        let response = send(&request).await?;
        match response.status {
            200 => response.read(usize::MAX).await,
            404 => Err(self.missing(WorkspacesErrorKind::Other, "command").await),
            _ => Err(response.failure("run command").await),
        }
    }

    async fn exists(&self) -> Result<bool, WorkspacesError> {
        let response = send(&Request::new(Method::Get, self.path())).await?;
        match response.status {
            200 => Ok(true),
            404 => Ok(false),
            _ => Err(response.failure(&format!("get sprite {}", self.name)).await),
        }
    }

    async fn missing(&self, kind: WorkspacesErrorKind, target: &str) -> WorkspacesError {
        match self.exists().await {
            Ok(true) => crate::error(
                kind,
                format!("{target} was not found on sprite {}", self.name),
            ),
            Ok(false) => crate::error(
                WorkspacesErrorKind::NotFound,
                format!("sprite {} was not found", self.name),
            ),
            Err(error) => error,
        }
    }

    fn path(&self) -> String {
        format!("/v1/sprites/{}", self.name)
    }
}

impl Request {
    const fn new(method: Method, path: String) -> Self {
        Self {
            method,
            path,
            content_type: None,
            body: Vec::new(),
            timeouts: None,
        }
    }

    fn with_body(mut self, content_type: &'static str, body: Vec<u8>) -> Self {
        self.content_type = Some(content_type);
        self.body = body;
        self
    }
}

impl Response {
    /// Reads the body, stopping once it exceeds the limit.
    async fn read(self, limit: usize) -> Result<Vec<u8>, WorkspacesError> {
        let Self {
            status: _,
            mut body,
            trailers,
        } = self;
        let mut bytes = Vec::new();
        loop {
            let (status, chunk) = body.read(Vec::with_capacity(16 * 1024)).await;
            bytes.extend(chunk);
            if bytes.len() > limit {
                return Ok(bytes);
            }
            if matches!(status, StreamResult::Dropped) {
                break;
            }
        }
        drop(body);
        trailers.await.map_err(|error| {
            crate::error(
                WorkspacesErrorKind::Other,
                format!("could not read Sprites response: {error}"),
            )
        })?;
        Ok(bytes)
    }

    async fn failure(self, operation: &str) -> WorkspacesError {
        let status = self.status;
        let message = match self.read(4096).await {
            Ok(body) => String::from_utf8_lossy(&body).trim().to_owned(),
            Err(error) => error.message,
        };
        crate::error(
            WorkspacesErrorKind::Other,
            format!("could not {operation}: Sprites returned HTTP {status}: {message}"),
        )
    }
}

pub fn query(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(name, value)| {
            format!(
                "{name}={}",
                percent_encoding::utf8_percent_encode(value, QUERY),
            )
        })
        .collect::<Vec<_>>()
        .join("&")
}

async fn send(request: &Request) -> Result<Response, WorkspacesError> {
    let key = key().await?;
    let response = send_with(request, &key).await?;
    if response.status != 401 {
        return Ok(response);
    }
    if credentials::refresh(CONNECTION.into(), key)
        .await
        .map_err(other)?
    {
        let response = send_with(request, &self::key().await?).await?;
        if response.status != 401 {
            return Ok(response);
        }
    }
    Err(crate::error(
        WorkspacesErrorKind::Other,
        "Sprites rejected the API key",
    ))
}

async fn key() -> Result<String, WorkspacesError> {
    match credentials::get(CONNECTION.into()).await.map_err(other)? {
        Some(Credential::ApiKey(key)) => Ok(key),
        Some(Credential::Oauth(credential)) => Ok(credential.access_token),
        None => Err(crate::error(
            WorkspacesErrorKind::Other,
            "Sprites is not signed in",
        )),
    }
}

async fn send_with(request: &Request, key: &str) -> Result<Response, WorkspacesError> {
    let mut headers = vec![(
        "authorization".to_owned(),
        format!("Bearer {key}").into_bytes(),
    )];
    if let Some(content_type) = request.content_type {
        headers.push(("content-type".to_owned(), content_type.as_bytes().to_vec()));
    }
    let headers = Fields::from_list(&headers).map_err(|error| {
        crate::error(
            WorkspacesErrorKind::Other,
            format!("invalid Sprites request headers: {error}"),
        )
    })?;
    let options = request.timeouts.as_ref().map(|timeouts| {
        let options = RequestOptions::new();
        options
            .set_first_byte_timeout(Some(timeouts.first_byte))
            .and_then(|()| options.set_between_bytes_timeout(Some(timeouts.between_bytes)))
            .map(|()| options)
    });
    let options = options.transpose().map_err(|error| {
        crate::error(
            WorkspacesErrorKind::Other,
            format!("invalid Sprites request timeout: {error}"),
        )
    })?;
    let (mut writer, contents) = bindings::wit_stream::new();
    let (_, trailers) = bindings::wit_future::new(|| Ok(None));
    let (http, _) = HttpRequest::new(headers, Some(contents), trailers, options);
    http.set_method(&request.method).expect("method is valid");
    http.set_scheme(Some(&Scheme::Https))
        .expect("scheme is valid");
    http.set_authority(Some(AUTHORITY))
        .expect("authority is valid");
    http.set_path_with_query(Some(&request.path))
        .expect("path is encoded");
    let body = request.body.clone();
    let ((), response) = future::join(
        async move {
            writer.write_all(body).await;
            drop(writer);
        },
        client::send(http),
    )
    .await;
    let response = response.map_err(|error| {
        crate::error(
            WorkspacesErrorKind::Other,
            format!("could not reach Sprites: {error}"),
        )
    })?;
    let status = response.get_status_code();
    let (_, result) = bindings::wit_future::new(|| Ok(()));
    let (body, trailers) = bindings::wasi::http::types::Response::consume_body(response, result);
    Ok(Response {
        status,
        body,
        trailers,
    })
}

fn other(message: String) -> WorkspacesError {
    crate::error(WorkspacesErrorKind::Other, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_encodes_reserved_characters() {
        let query = query(&[("path", "/home/sprite/a b&c=d+é"), ("mkdir", "true")]);

        assert_eq!(
            query,
            "path=%2Fhome%2Fsprite%2Fa%20b%26c%3Dd%2B%C3%A9&mkdir=true",
        );
    }
}
