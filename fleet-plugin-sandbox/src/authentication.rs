use crate::{
    Config, Sandbox,
    bindings::exports::fleet::providers::authentication::{
        AuthenticationKind, AuthenticationMethod, Connection, Guest, OauthConfig,
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
            methods.push(AuthenticationMethod {
                name: "oauth".into(),
                title: "Sign in".into(),
                kind: AuthenticationKind::Oauth(OauthConfig {
                    authorization_url: format!("{base}/authorize"),
                    token_url: format!("{base}/token"),
                    client_id: "fleet-sandbox".into(),
                    redirect_uri: "http://127.0.0.1:0/callback".into(),
                    scopes: vec!["sandbox".into()],
                    parameters: Vec::new(),
                    resource: None,
                    issuer: None,
                    registration: None,
                    required_scopes: Vec::new(),
                }),
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
