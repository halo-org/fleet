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
/// A host starting with `*.`, such as `https://*.example.com`, matches subdomains
/// at any depth but not the domain itself. Its domain must end in a label starting
/// with a letter, so it never matches an IP address.
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
            && uri.host().is_some_and(|host| self.allows_host(host))
            && port(uri) == Some(self.port)
    }

    fn allows_host(&self, host: &str) -> bool {
        let Some(suffix) = self.host.strip_prefix('*') else {
            return host.eq_ignore_ascii_case(&self.host);
        };
        host.len()
            .checked_sub(suffix.len())
            .filter(|&start| start > 0)
            .and_then(|start| host.get(start..))
            .is_some_and(|end| end.eq_ignore_ascii_case(suffix))
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
        let host = authority.host().to_ascii_lowercase();
        if authority.as_str().contains('@')
            || (origin.contains('*')
                && (origin.matches('*').count() > 1
                    || !host.strip_prefix("*.").is_some_and(is_domain)))
            || !matches!(
                uri.path_and_query().map(PathAndQuery::as_str),
                None | Some("/")
            )
        {
            return Err(invalid());
        }
        Ok(Self {
            scheme: scheme.clone(),
            host,
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

fn is_domain(domain: &str) -> bool {
    let domain = domain.strip_suffix('.').unwrap_or(domain);
    domain.split('.').all(|label| !label.is_empty())
        && domain
            .rsplit('.')
            .next()
            .is_some_and(|label| label.starts_with(|c: char| c.is_ascii_alphabetic()))
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
    use core::assert_matches;
    use futures_util::FutureExt as _;

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
        assert!(!allows("https://api.example.com./"));
        assert!(!allows("/v1"));
    }

    #[test]
    fn wildcard_origins_parse_from_a_leading_label() {
        assert_eq!(
            "HTTPS://*.Example.com/".parse::<WasmOrigin>().unwrap(),
            "https://*.example.com:443".parse().unwrap(),
        );
        for origin in [
            "https://*",
            "https://*.",
            "https://*.:443",
            "https://*..example.com",
            "https://*example.com",
            "https://api-*.example.com",
            "https://a.*.example.com",
            "https://*.*.example.com",
            "https://*.example.*",
            "https://*.0.0.1",
            "https://*.example.com:844*",
            "https://*.example.com/#*",
            "https://*.example.com/v1",
            "https://user@*.example.com",
            "*.example.com",
        ] {
            let error = origin.parse::<WasmOrigin>().unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("invalid HTTP origin \"{origin}\"")
            );
        }
    }

    #[test]
    fn wildcard_origins_allow_subdomains() {
        let origin = "https://*.example.com".parse::<WasmOrigin>().unwrap();
        let allows = |uri: &str| origin.allows(&uri.parse().unwrap());

        assert!(allows("https://api.example.com/v1"));
        assert!(allows("https://a.b.EXAMPLE.com/"));
        assert!(!allows("https://example.com/"));
        assert!(!allows("https://badexample.com/"));
        assert!(!allows("https://api.example.com.evil.net/"));
        assert!(!allows("https://api.example.com./"));
        assert!(!allows("https://127.0.0.1/"));
        assert!(!allows("http://api.example.com/"));
        assert!(!allows("https://api.example.com:8443/"));
    }

    #[test]
    fn wildcard_origins_keep_ports_and_trailing_dots() {
        let origin = "https://*.example.com.:8443".parse::<WasmOrigin>().unwrap();
        let allows = |uri: &str| origin.allows(&uri.parse().unwrap());

        assert!(allows("https://api.example.com.:8443/"));
        assert!(!allows("https://api.example.com:8443/"));
        assert!(!allows("https://api.example.com./"));
    }

    #[test]
    fn requests_are_denied_without_origins() {
        let request = Request::get("https://api.example.com/")
            .body(WasiBody::default())
            .unwrap();

        let response =
            WasmOrigins::default().send_request(request, None, Box::new(async { Ok(()) }));
        let error = Box::into_pin(response)
            .now_or_never()
            .unwrap()
            .err()
            .unwrap();

        assert_matches!(error, Error::HttpRequestDenied);
    }
}
