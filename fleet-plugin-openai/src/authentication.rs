use crate::{
    OpenAi,
    bindings::{
        exports::fleet::providers::authentication::{
            AuthenticationKind, AuthenticationMethod, Connection, Guest, OauthConfig,
            OauthRegistration,
        },
        fleet::providers::credentials::{self, Credential},
    },
};
use fleet_models::Credentials;
use http::{HeaderMap, HeaderValue, header};
use std::io;

const CONNECTION: &str = "chatgpt";

pub struct HostCredentials;

impl Guest for OpenAi {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn connections(host_id: String) -> Result<Vec<Connection>, String> {
        Ok(vec![Connection {
            name: CONNECTION.into(),
            title: "ChatGPT".into(),
            description: Some(
                "Eligible requests use your ChatGPT plan. Manage usage and limits in ChatGPT settings."
                    .into(),
            ),
            manage_url: Some("https://chatgpt.com/settings/usage".into()),
            methods: vec![AuthenticationMethod {
                name: CONNECTION.into(),
                title: "Continue with ChatGPT".into(),
                kind: AuthenticationKind::Oauth(OauthConfig {
                    authorization_url: "https://auth.openai.com/api/accounts/authorize".into(),
                    token_url: "https://auth.openai.com/api/accounts/oauth/token".into(),
                    client_id: "dynamic_agent_client".into(),
                    redirect_uri: "http://127.0.0.1:0/auth/callback".into(),
                    scopes: [
                        "openid",
                        "profile",
                        "email",
                        "offline_access",
                        "resource.invoke",
                        "chatgpt.tokens.use.direct",
                    ]
                    .map(str::to_owned)
                    .to_vec(),
                    parameters: vec![("ext_agent_host_id".into(), host_id)],
                    resource: Some("https://api.openai.com/v1".into()),
                    issuer: Some("https://auth.openai.com".into()),
                    registration: Some(OauthRegistration {
                        client_id_parameter: "client_id".into(),
                        parameters: vec![("agent_name_hint".into(), "Fleet".into())],
                    }),
                    required_scopes: vec!["chatgpt.tokens.use.direct".into()],
                }),
            }],
        }])
    }
}

impl Credentials for HostCredentials {
    type Error = io::Error;

    async fn get(&self) -> Result<HeaderMap, Self::Error> {
        let credential = credentials::get(CONNECTION.into())
            .await
            .map_err(io::Error::other)?
            .ok_or_else(|| io::Error::other("Sign in with ChatGPT in Fleet connections"))?;
        let token = match credential {
            Credential::Oauth(token) => token.access_token,
            Credential::ApiKey(key) => key,
        };
        let mut value =
            HeaderValue::from_str(&format!("Bearer {token}")).map_err(io::Error::other)?;
        value.set_sensitive(true);
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, value);
        Ok(headers)
    }

    async fn refresh(&self, rejected: &HeaderMap) -> Result<bool, Self::Error> {
        let Some(token) = rejected
            .get(header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.strip_prefix("Bearer "))
        else {
            return Ok(false);
        };
        credentials::refresh(CONNECTION.into(), token.into())
            .await
            .map_err(io::Error::other)
    }
}
