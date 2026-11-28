use crate::{AuthenticationError, oauth::Discovered};
use serde::Deserialize;
use url::Url;

/// Protected resource metadata, defined by RFC 9728.
#[derive(Deserialize)]
pub struct ResourceMetadata {
    resource: String,
    #[serde(default)]
    authorization_servers: Vec<String>,
    #[serde(default)]
    scopes_supported: Vec<String>,
}

/// Authorization server metadata, defined by RFC 8414.
#[derive(Deserialize)]
pub struct ServerMetadata {
    issuer: String,
    authorization_endpoint: Option<String>,
    token_endpoint: Option<String>,
    revocation_endpoint: Option<String>,
    #[serde(default)]
    code_challenge_methods_supported: Vec<String>,
}

impl ResourceMetadata {
    /// Returns the first authorization server's issuer and the supported scopes.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata describes another resource or lists no server.
    pub fn issuer(self, resource: &str) -> Result<(String, Vec<String>), AuthenticationError> {
        if self.resource != resource {
            return Err(AuthenticationError::Invalid(
                "protected resource metadata describes a different resource",
            ));
        }
        let issuer =
            self.authorization_servers
                .into_iter()
                .next()
                .ok_or(AuthenticationError::Invalid(
                    "protected resource metadata lists no authorization server",
                ))?;
        Ok((issuer, self.scopes_supported))
    }
}

impl ServerMetadata {
    /// Returns the server for the resource and its authorization endpoint.
    ///
    /// # Errors
    ///
    /// Returns an error if the metadata describes another issuer, lacks an endpoint or
    /// does not support PKCE with S256.
    pub fn server(
        self,
        resource: &str,
        issuer: &str,
    ) -> Result<(Discovered, String), AuthenticationError> {
        if self.issuer != issuer {
            return Err(AuthenticationError::Invalid(
                "authorization server metadata describes a different issuer",
            ));
        }
        let (Some(authorization_endpoint), Some(token_endpoint)) =
            (self.authorization_endpoint, self.token_endpoint)
        else {
            return Err(AuthenticationError::Invalid(
                "authorization server metadata lacks an authorization or token endpoint",
            ));
        };
        if !self
            .code_challenge_methods_supported
            .iter()
            .any(|method| method == "S256")
        {
            return Err(AuthenticationError::Invalid(
                "authorization server does not support PKCE with S256",
            ));
        }
        let server = Discovered {
            resource: resource.into(),
            issuer: self.issuer,
            token_endpoint,
            revocation_endpoint: self.revocation_endpoint,
        };
        Ok((server, authorization_endpoint))
    }
}

/// Returns the URLs of a resource's metadata in the order to try them.
pub fn resource_metadata_urls(resource: &Url) -> Vec<Url> {
    let mut urls = vec![well_known(resource, "oauth-protected-resource")];
    let mut root = resource.clone();
    root.set_path("/.well-known/oauth-protected-resource");
    root.set_query(None);
    if !urls.contains(&root) {
        urls.push(root);
    }
    urls
}

/// Returns the URLs of an authorization server's metadata in the order to try them.
pub fn server_metadata_urls(issuer: &Url) -> Vec<Url> {
    let mut urls = vec![
        well_known(issuer, "oauth-authorization-server"),
        well_known(issuer, "openid-configuration"),
    ];
    if !path(issuer).is_empty() {
        let mut url = issuer.clone();
        url.set_path(&format!(
            "{}/.well-known/openid-configuration",
            path(issuer)
        ));
        urls.push(url);
    }
    urls
}

fn well_known(url: &Url, name: &str) -> Url {
    let mut result = url.clone();
    result.set_path(&format!("/.well-known/{name}{}", path(url)));
    result
}

fn path(url: &Url) -> &str {
    url.path().trim_end_matches('/')
}

#[cfg(test)]
mod tests {
    use super::*;
    use core::assert_matches;
    use serde_json::{Value, json};

    fn urls(urls: Vec<Url>) -> Vec<String> {
        urls.into_iter().map(String::from).collect()
    }

    fn server(metadata: Value) -> Result<(Discovered, String), AuthenticationError> {
        serde_json::from_value::<ServerMetadata>(metadata)
            .unwrap()
            .server(
                "https://api.example.com/mcp",
                "https://auth.example.com/tenant",
            )
    }

    #[test]
    fn resource_metadata_is_found_below_the_resource_path_then_at_the_root() {
        for (resource, expected) in [
            (
                "https://api.example.com/mcp",
                vec![
                    "https://api.example.com/.well-known/oauth-protected-resource/mcp",
                    "https://api.example.com/.well-known/oauth-protected-resource",
                ],
            ),
            (
                "https://api.example.com/v1/mcp/",
                vec![
                    "https://api.example.com/.well-known/oauth-protected-resource/v1/mcp",
                    "https://api.example.com/.well-known/oauth-protected-resource",
                ],
            ),
            (
                "https://api.example.com:8443/mcp?tenant=a",
                vec![
                    "https://api.example.com:8443/.well-known/oauth-protected-resource/mcp?tenant=a",
                    "https://api.example.com:8443/.well-known/oauth-protected-resource",
                ],
            ),
            (
                "https://api.example.com",
                vec!["https://api.example.com/.well-known/oauth-protected-resource"],
            ),
            (
                "https://api.example.com/",
                vec!["https://api.example.com/.well-known/oauth-protected-resource"],
            ),
            (
                "https://api.example.com/?tenant=a",
                vec![
                    "https://api.example.com/.well-known/oauth-protected-resource?tenant=a",
                    "https://api.example.com/.well-known/oauth-protected-resource",
                ],
            ),
        ] {
            assert_eq!(
                urls(resource_metadata_urls(&resource.parse().unwrap())),
                expected
            );
        }
    }

    #[test]
    fn server_metadata_is_found_through_oauth_then_openid_locations() {
        for (issuer, expected) in [
            (
                "https://auth.example.com/tenant",
                vec![
                    "https://auth.example.com/.well-known/oauth-authorization-server/tenant",
                    "https://auth.example.com/.well-known/openid-configuration/tenant",
                    "https://auth.example.com/tenant/.well-known/openid-configuration",
                ],
            ),
            (
                "https://auth.example.com/tenant/",
                vec![
                    "https://auth.example.com/.well-known/oauth-authorization-server/tenant",
                    "https://auth.example.com/.well-known/openid-configuration/tenant",
                    "https://auth.example.com/tenant/.well-known/openid-configuration",
                ],
            ),
            (
                "https://auth.example.com",
                vec![
                    "https://auth.example.com/.well-known/oauth-authorization-server",
                    "https://auth.example.com/.well-known/openid-configuration",
                ],
            ),
        ] {
            assert_eq!(
                urls(server_metadata_urls(&issuer.parse().unwrap())),
                expected
            );
        }
    }

    #[test]
    fn resource_metadata_names_the_first_server_of_the_same_resource() {
        let metadata = |value| serde_json::from_value::<ResourceMetadata>(value).unwrap();
        let (issuer, scopes) = metadata(json!({
            "resource": "https://api.example.com/mcp",
            "authorization_servers": ["https://auth.example.com", "https://other.example.com"],
            "scopes_supported": ["read", "write"],
        }))
        .issuer("https://api.example.com/mcp")
        .unwrap();

        assert_eq!(issuer, "https://auth.example.com");
        assert_eq!(scopes, ["read", "write"]);

        for (value, message) in [
            (
                json!({
                    "resource": "https://api.example.com/mcp/",
                    "authorization_servers": ["https://auth.example.com"],
                }),
                "protected resource metadata describes a different resource",
            ),
            (
                json!({
                    "resource": "https://api.example.com/mcp",
                    "authorization_servers": [],
                }),
                "protected resource metadata lists no authorization server",
            ),
            (
                json!({ "resource": "https://api.example.com/mcp" }),
                "protected resource metadata lists no authorization server",
            ),
        ] {
            let error = metadata(value)
                .issuer("https://api.example.com/mcp")
                .unwrap_err();
            assert_matches!(error, AuthenticationError::Invalid(error) if error == message);
        }
    }

    #[test]
    fn server_metadata_describes_the_issuer_with_pkce() {
        let (discovered, authorization) = server(json!({
            "issuer": "https://auth.example.com/tenant",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token",
            "revocation_endpoint": "https://auth.example.com/revoke",
            "code_challenge_methods_supported": ["plain", "S256"],
        }))
        .unwrap();

        assert_eq!(authorization, "https://auth.example.com/authorize");
        assert_eq!(discovered.resource, "https://api.example.com/mcp");
        assert_eq!(discovered.issuer, "https://auth.example.com/tenant");
        assert_eq!(discovered.token_endpoint, "https://auth.example.com/token");
        assert_eq!(
            discovered.revocation_endpoint.as_deref(),
            Some("https://auth.example.com/revoke")
        );

        let valid = json!({
            "issuer": "https://auth.example.com/tenant",
            "authorization_endpoint": "https://auth.example.com/authorize",
            "token_endpoint": "https://auth.example.com/token",
            "code_challenge_methods_supported": ["S256"],
        });
        let without = |field: &str| {
            let mut metadata = valid.clone();
            metadata.as_object_mut().unwrap().remove(field);
            metadata
        };
        let mut other_issuer = valid.clone();
        other_issuer["issuer"] = "https://auth.example.com/tenant/".into();
        let mut plain = valid.clone();
        plain["code_challenge_methods_supported"] = json!(["plain"]);
        for (metadata, message) in [
            (
                other_issuer,
                "authorization server metadata describes a different issuer",
            ),
            (
                without("authorization_endpoint"),
                "authorization server metadata lacks an authorization or token endpoint",
            ),
            (
                without("token_endpoint"),
                "authorization server metadata lacks an authorization or token endpoint",
            ),
            (
                without("code_challenge_methods_supported"),
                "authorization server does not support PKCE with S256",
            ),
            (
                plain,
                "authorization server does not support PKCE with S256",
            ),
        ] {
            let error = server(metadata).err().unwrap();
            assert_matches!(error, AuthenticationError::Invalid(error) if error == message);
        }
    }
}
