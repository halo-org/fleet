use crate::WasmError;
use core::str::FromStr;
use http::{
    Request, Uri,
    uri::{PathAndQuery, Scheme},
};
use std::sync::Arc;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

/// An origin a WebAssembly provider may send HTTP requests to.
///
/// Parses from a URL with an `http` or `https` scheme, a host and an optional port,
/// such as `https://api.example.com`. Omitted ports match the scheme's default port.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WasmOrigin {
    scheme: Scheme,
    host: String,
    port: u16,
}

#[derive(Clone, Default)]
pub struct WasmOrigins(Arc<[WasmOrigin]>);

type SendRequest = Box<
    dyn Future<
            Output = Result<
                (
                    http::Response<WasiBody>,
                    Box<dyn Future<Output = Result<(), Error>> + Send>,
                ),
                Error,
            >,
        > + Send,
>;

impl WasmOrigin {
    fn allows(&self, uri: &Uri) -> bool {
        uri.scheme() == Some(&self.scheme)
            && uri
                .host()
                .is_some_and(|host| host.eq_ignore_ascii_case(&self.host))
            && port(uri) == Some(self.port)
    }
}

impl FromStr for WasmOrigin {
    type Err = WasmError;

    fn from_str(origin: &str) -> Result<Self, Self::Err> {
        let invalid = || WasmError::invalid_origin(origin);
        let uri = origin.parse::<Uri>().map_err(|_| invalid())?;
        let scheme = uri
            .scheme()
            .filter(|scheme| [Scheme::HTTP, Scheme::HTTPS].contains(scheme))
            .ok_or_else(invalid)?;
        let authority = uri.authority().ok_or_else(invalid)?;
        if authority.as_str().contains('@')
            || !matches!(
                uri.path_and_query().map(PathAndQuery::as_str),
                None | Some("/")
            )
        {
            return Err(invalid());
        }
        Ok(Self {
            scheme: scheme.clone(),
            host: authority.host().to_ascii_lowercase(),
            port: port(&uri).ok_or_else(invalid)?,
        })
    }
}

impl WasmOrigins {
    pub fn new(origins: Vec<WasmOrigin>) -> Self {
        Self(origins.into())
    }
}

impl WasiHttpHooks for WasmOrigins {
    fn send_request(
        &mut self,
        request: Request<WasiBody>,
        options: Option<RequestOptions>,
        fut: Box<dyn Future<Output = Result<(), Error>> + Send>,
    ) -> SendRequest {
        if self.0.iter().any(|origin| origin.allows(request.uri())) {
            wasmtime_wasi_http::default_hooks().send_request(request, options, fut)
        } else {
            Box::new(async { Err(Error::HttpRequestDenied) })
        }
    }
}

fn port(uri: &Uri) -> Option<u16> {
    uri.port_u16().or_else(|| match uri.scheme_str()? {
        "https" => Some(443),
        "http" => Some(80),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn origins_parse_from_urls_without_paths() {
        assert_eq!(
            "HTTPS://API.Example.com/".parse::<WasmOrigin>().unwrap(),
            "https://api.example.com:443".parse().unwrap(),
        );
        for origin in [
            "api.example.com",
            "/v1",
            "https://api.example.com/v1",
            "https://api.example.com?key=value",
            "https://user@api.example.com",
            "ftp://example.com:21",
        ] {
            let error = origin.parse::<WasmOrigin>().unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("invalid HTTP origin \"{origin}\"")
            );
        }
    }

    #[test]
    fn origins_allow_matching_scheme_host_and_port() {
        let origin = "https://api.example.com".parse::<WasmOrigin>().unwrap();
        let allows = |uri: &str| origin.allows(&uri.parse().unwrap());

        assert!(allows("https://api.example.com/v1/sprites?name=a"));
        assert!(allows("https://API.example.com:443/"));
        assert!(!allows("http://api.example.com/"));
        assert!(!allows("https://api.example.com:8443/"));
        assert!(!allows("https://example.com/"));
        assert!(!allows("https://api.example.com.evil.com/"));
        assert!(!allows("/v1"));
    }
}
