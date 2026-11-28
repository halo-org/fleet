use crate::{
    Sprites, WorkspacesError, WorkspacesErrorKind,
    bindings::{
        exports::fleet::providers::authentication::{
            AuthenticationKind, AuthenticationMethod, Connection, Guest,
        },
        fleet::host::credentials::{self, Credential},
    },
    config::ProviderConfig,
};

impl Guest for Sprites {
    #[expect(clippy::unused_async_trait_impl, reason = "matches the WIT export")]
    async fn connections(_host_id: String) -> Result<Vec<Connection>, String> {
        let config = ProviderConfig::load()?;
        let mut connections = vec![Connection {
            name: crate::sprites::CONNECTION.into(),
            title: "Sprites".into(),
            description: Some("Runs workspaces in Sprites VMs.".into()),
            manage_url: Some("https://sprites.dev".into()),
            methods: methods(),
        }];
        connections.extend(
            config
                .connections
                .into_iter()
                .map(|(name, connection)| Connection {
                    name,
                    title: connection.title,
                    description: connection.description,
                    manage_url: None,
                    methods: methods(),
                }),
        );
        Ok(connections)
    }
}

/// Returns the current credential of a connection.
pub async fn credential(connection: &str) -> Result<Option<String>, WorkspacesError> {
    let credential = credentials::get(connection.into())
        .await
        .map_err(|message| crate::error(WorkspacesErrorKind::Other, message))?;
    Ok(credential.map(|credential| match credential {
        Credential::ApiKey(key) => key,
        Credential::Oauth(credential) => credential.access_token,
    }))
}

fn methods() -> Vec<AuthenticationMethod> {
    vec![AuthenticationMethod {
        name: "api-key".into(),
        title: "API key".into(),
        kind: AuthenticationKind::ApiKey,
    }]
}
