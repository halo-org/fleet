use crate::{
    Config, Sandbox,
    bindings::exports::fleet::providers::authentication::{
        AuthenticationKind, AuthenticationMethod, Connection, Guest, OauthClient, OauthConfig,
        OauthEndpoints, OauthRegisteredClient, OauthServer,
    },
};

impl Guest for Sandbox {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn connections(_host_id: String) -> Result<Vec<Connection>, String> {
        let config = Config::load()?;
        let mut methods = vec![AuthenticationMethod {
            name: "key".into(),
            title: "API key".into(),
            kind: AuthenticationKind::ApiKey,
        }];
        if let Some(oauth) = config.oauth {
            let base = oauth.trim_end_matches('/');
            let endpoints = OauthEndpoints {
                authorization_url: format!("{base}/authorize"),
                token_url: format!("{base}/token"),
                resource: None,
                issuer: None,
            };
            let client = OauthClient::Registered(OauthRegisteredClient {
                client_id: "fleet-sandbox".into(),
                redirect_uri: "http://127.0.0.1:0/callback".into(),
                registration: None,
            });
            methods.push(AuthenticationMethod {
                name: "oauth".into(),
                title: "Sign in".into(),
                kind: oauth_kind(
                    OauthServer::Endpoints(endpoints),
                    client,
                    vec!["sandbox".into()],
                ),
            });
            let resource = format!("{base}/resource");
            methods.push(AuthenticationMethod {
                name: "resource".into(),
                title: "Sign in to the resource".into(),
                kind: oauth_kind(
                    OauthServer::Resource(resource),
                    OauthClient::Automatic,
                    Vec::new(),
                ),
            });
        }
        Ok(vec![Connection {
            name: crate::CONNECTION.into(),
            title: "Sandbox".into(),
            description: Some("A sandbox account. Any API key works.".into()),
            manage_url: None,
            methods,
        }])
    }
}

const fn oauth_kind(
    server: OauthServer,
    client: OauthClient,
    scopes: Vec<String>,
) -> AuthenticationKind {
    AuthenticationKind::Oauth(OauthConfig {
        server,
        client,
        scopes,
        parameters: Vec::new(),
        required_scopes: Vec::new(),
    })
}
