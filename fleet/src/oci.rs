use core::error::Error;
use reqwest::{
    Client, Response, StatusCode,
    header::{ACCEPT, WWW_AUTHENTICATE},
};
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::{collections::BTreeMap, io};
use url::Url;

const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const WASM_CONFIG: &str = "application/vnd.wasm.config.v0+json";
const WASM_LAYER: &str = "application/wasm";

#[derive(Debug, PartialEq, Eq)]
struct Reference<'a> {
    registry: &'a str,
    repository: &'a str,
    version: &'a str,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Manifest {
    config: Descriptor,
    layers: Vec<Descriptor>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Descriptor {
    media_type: String,
    digest: String,
}

#[derive(Deserialize)]
struct TokenResponse {
    token: String,
}

struct Registry<'a> {
    client: &'a Client,
    base: Url,
    repository: &'a str,
    token: Option<String>,
}

/// Pulls a Wasm component from a reference such as `ghcr.io/owner/name:tag`.
pub async fn pull(
    client: &Client,
    reference: &str,
) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
    let reference = Reference::parse(reference).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid OCI reference: {reference}"),
        )
    })?;
    let mut registry = Registry {
        client,
        base: Url::parse(&format!("https://{}/v2/", reference.registry))?,
        repository: reference.repository,
        token: None,
    };

    let bytes = registry
        .get(&format!("manifests/{}", reference.version), MANIFEST)
        .await?;
    if reference.version.starts_with("sha256:") {
        verify(&bytes, reference.version)?;
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    let [layer] = manifest.layers.as_slice() else {
        return Err(not_component());
    };
    if manifest.config.media_type != WASM_CONFIG || layer.media_type != WASM_LAYER {
        return Err(not_component());
    }

    let bytes = registry
        .get(&format!("blobs/{}", layer.digest), WASM_LAYER)
        .await?;
    verify(&bytes, &layer.digest)?;
    Ok(bytes)
}

impl<'a> Reference<'a> {
    fn parse(reference: &'a str) -> Option<Self> {
        let (registry, path) = reference.split_once('/')?;
        let (repository, version) = if let Some((repository, digest)) = path.split_once('@') {
            let hex = digest.strip_prefix("sha256:")?;
            (hex.len() == 64 && consists_of(hex, |byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f')))
                .then_some((repository, digest))?
        } else {
            let (repository, tag) = path.rsplit_once(':')?;
            (tag.len() <= 128
                && consists_of(tag, |byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')
                }))
            .then_some((repository, tag))?
        };
        let valid_registry = consists_of(registry, |byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b':')
        });
        let valid_repository = consists_of(
            repository,
            |byte| matches!(byte, b'a'..=b'z' | b'0'..=b'9' | b'.' | b'_' | b'-' | b'/'),
        );
        (valid_registry && valid_repository).then_some(Self {
            registry,
            repository,
            version,
        })
    }
}

impl Registry<'_> {
    async fn get(
        &mut self,
        path: &str,
        accept: &str,
    ) -> Result<Vec<u8>, Box<dyn Error + Send + Sync>> {
        let url = self.base.join(&format!("{}/{path}", self.repository))?;
        let mut response = self.send(&url, accept).await?;
        if response.status() == StatusCode::UNAUTHORIZED && self.token.is_none() {
            self.token = Some(self.token(&response).await?);
            response = self.send(&url, accept).await?;
        }
        Ok(response.error_for_status()?.bytes().await?.to_vec())
    }

    async fn send(&self, url: &Url, accept: &str) -> reqwest::Result<Response> {
        let request = self.client.get(url.clone()).header(ACCEPT, accept);
        match &self.token {
            Some(token) => request.bearer_auth(token),
            None => request,
        }
        .send()
        .await
    }

    async fn token(&self, response: &Response) -> Result<String, Box<dyn Error + Send + Sync>> {
        let challenge = response
            .headers()
            .get(WWW_AUTHENTICATE)
            .and_then(|value| value.to_str().ok())
            .and_then(bearer_challenge)
            .ok_or_else(|| io::Error::other("the registry requires unsupported authentication"))?;
        let realm = challenge
            .get("realm")
            .ok_or_else(|| io::Error::other("the registry challenge has no realm"))?;
        let mut url = Url::parse(realm)?;
        url.query_pairs_mut().extend_pairs(
            ["service", "scope"]
                .into_iter()
                .filter_map(|key| Some((key, challenge.get(key)?))),
        );
        let bytes = self
            .client
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        Ok(serde_json::from_slice::<TokenResponse>(&bytes)?.token)
    }
}

fn bearer_challenge(header: &str) -> Option<BTreeMap<String, &str>> {
    let (scheme, mut rest) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let mut parameters = BTreeMap::new();
    loop {
        let (key, value) = rest.split_once('=')?;
        let (value, next) = match value.strip_prefix('"') {
            Some(value) => {
                let (value, next) = value.split_once('"')?;
                (value, next.trim_start().strip_prefix(',').unwrap_or(next))
            }
            None => value.split_once(',').unwrap_or((value, "")),
        };
        parameters.insert(key.trim().to_ascii_lowercase(), value);
        rest = next.trim_start();
        if rest.is_empty() {
            return Some(parameters);
        }
    }
}

fn consists_of(value: &str, allowed: impl Fn(u8) -> bool) -> bool {
    !value.is_empty() && value.bytes().all(allowed)
}

fn verify(bytes: &[u8], digest: &str) -> io::Result<()> {
    if digest.strip_prefix("sha256:") != Some(&format!("{:x}", Sha256::digest(bytes))) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("registry content does not match {digest}"),
        ));
    }
    Ok(())
}

fn not_component() -> Box<dyn Error + Send + Sync> {
    io::Error::new(
        io::ErrorKind::InvalidData,
        "the OCI artifact is not a Wasm component",
    )
    .into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reference_requires_registry_repository_and_version() {
        let digest = format!("sha256:{}", "a".repeat(64));

        assert_eq!(
            Reference::parse("ghcr.io/owner/name:v1.0").unwrap(),
            Reference {
                registry: "ghcr.io",
                repository: "owner/name",
                version: "v1.0",
            },
        );
        assert_eq!(
            Reference::parse(&format!("localhost:5000/name@{digest}")).unwrap(),
            Reference {
                registry: "localhost:5000",
                repository: "name",
                version: &digest,
            },
        );
        for reference in [
            "ghcr.io/owner/name",
            "ghcr.io/owner/Name:v1",
            "ghcr.io/owner/name:v?1",
            "ghcr.io/owner/name@sha256:abc",
            "user@ghcr.io/owner/name:v1",
            "name:v1",
        ] {
            assert_eq!(Reference::parse(reference), None);
        }
    }

    #[test]
    fn bearer_challenge_reads_quoted_parameters() {
        let challenge = bearer_challenge(
            r#"Bearer realm="https://ghcr.io/token",service="ghcr.io",scope="repository:owner/name:pull,push""#,
        )
        .unwrap();

        assert_eq!(
            challenge,
            BTreeMap::from([
                ("realm".into(), "https://ghcr.io/token"),
                ("scope".into(), "repository:owner/name:pull,push"),
                ("service".into(), "ghcr.io"),
            ]),
        );
        assert_eq!(bearer_challenge(r#"Basic realm="registry""#), None);
    }
}
