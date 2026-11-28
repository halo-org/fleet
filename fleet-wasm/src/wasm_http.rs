use crate::WasmError;
use core::{
    net::{Ipv4Addr, Ipv6Addr},
    str::FromStr,
};
use http::{Request, Uri, uri::Scheme};
use std::sync::Arc;
use wasmtime_wasi_http::{Error, RequestOptions, WasiBody, WasiHttpHooks};

/// An origin a WebAssembly provider may send HTTP requests to.
///
/// Parses from `scheme://host` with an optional `:port` and an optional trailing `/`,
/// such as `https://api.example.com`. The scheme is `http` or `https`. The host is a
/// domain name of ASCII letters, digits, hyphens and underscores without a trailing
/// dot, an IPv4 address or an IPv6 address in brackets. Ports are decimal numbers from
/// 1 to 65535 without leading zeros. Omitted ports match the scheme's default port.
/// A host starting with `*.`, such as `https://*.example.com`, matches subdomains
/// at any depth but not the domain itself. Its domain must end in a label starting
/// with a letter, so it never matches an IP address. Hosts are compared as text,
/// ignoring ASCII case.
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
                .authority()
                .and_then(|authority| {
                    host_port(authority.as_str().rsplit('@').next()?, &self.scheme)
                })
                .is_some_and(|(host, port)| self.allows_host(host) && port == self.port)
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
        let (scheme, authority) = origin.split_once("://").ok_or_else(invalid)?;
        let scheme = [Scheme::HTTP, Scheme::HTTPS]
            .into_iter()
            .find(|known| known.as_str().eq_ignore_ascii_case(scheme))
            .ok_or_else(invalid)?;
        let authority = authority.strip_suffix('/').unwrap_or(authority);
        let (host, port) = host_port(authority, &scheme)
            .filter(|&(host, _)| is_host(host))
            .ok_or_else(invalid)?;
        Ok(Self {
            scheme,
            host: host.to_ascii_lowercase(),
            port,
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

fn host_port<'a>(authority: &'a str, scheme: &Scheme) -> Option<(&'a str, u16)> {
    let host = if authority.starts_with('[') {
        authority
            .split_inclusive(']')
            .next()
            .filter(|host| host.ends_with(']'))?
    } else {
        authority.split(':').next()?
    };
    let port = match authority.strip_prefix(host)? {
        "" => match scheme.as_str() {
            "http" => 80,
            "https" => 443,
            _ => return None,
        },
        port => parse_port(port.strip_prefix(':')?)?,
    };
    Some((host, port))
}

fn parse_port(port: &str) -> Option<u16> {
    if port.starts_with('0') || !port.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    port.parse().ok()
}

fn is_host(host: &str) -> bool {
    if let Some(domain) = host.strip_prefix("*.") {
        return is_domain(domain)
            && last_label(domain).starts_with(|c: char| c.is_ascii_alphabetic());
    }
    if let Some(address) = host.strip_prefix('[') {
        return address
            .strip_suffix(']')
            .is_some_and(|address| address.parse::<Ipv6Addr>().is_ok());
    }
    host.parse::<Ipv4Addr>().is_ok()
        || (is_domain(host) && !last_label(host).bytes().all(|byte| byte.is_ascii_digit()))
}

fn is_domain(domain: &str) -> bool {
    domain.len() <= 253
        && domain.split('.').all(|label| {
            (1..=63).contains(&label.len())
                && !label.starts_with('-')
                && !label.ends_with('-')
                && label
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        })
}

fn last_label(domain: &str) -> &str {
    domain.rsplit_once('.').map_or(domain, |(_, label)| label)
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
            "https://api.example.com//",
            "https://api.example.com?key=value",
            "https://api.example.com/?",
            "https://api.example.com#top",
            "https://user@api.example.com",
            "https://user:pw@api.example.com",
            "ftp://example.com:21",
            "ws://example.com",
            "https:/api.example.com",
            "https://",
            "https:///",
            "https://:443",
            " https://api.example.com",
            "https://api.example.com ",
            "https://api.example.com/\n",
        ] {
            let error = origin.parse::<WasmOrigin>().unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("invalid HTTP origin \"{origin}\"")
            );
        }
    }

    #[test]
    fn origins_parse_domains_and_ip_addresses() {
        let label = format!("https://{}.example.com", "a".repeat(63));
        let domain = format!("https://{}a", "a.".repeat(126));
        for (origin, parsed) in [
            ("https://api.example.com", "https://api.example.com:443"),
            ("https://api.example.com/", "https://api.example.com:443"),
            ("HtTpS://API.Example.io:8443", "https://api.example.io:8443"),
            ("http://localhost:1", "http://localhost:1"),
            ("http://my_api.internal:80", "http://my_api.internal:80"),
            ("https://x-1.example.com", "https://x-1.example.com:443"),
            (&label, &format!("{label}:443")),
            (&domain, &format!("{domain}:443")),
            ("https://*.example.com", "https://*.example.com:443"),
            ("https://*.Example.io:65535/", "https://*.example.io:65535"),
            ("http://127.0.0.1", "http://127.0.0.1:80"),
            ("http://127.0.0.1:8080/", "http://127.0.0.1:8080"),
            ("http://[::1]", "http://[::1]:80"),
            ("HTTP://[FE80::1]:8443/", "http://[fe80::1]:8443"),
            ("https://[::ffff:1.2.3.4]", "https://[::ffff:1.2.3.4]:443"),
        ] {
            let origin = origin.parse::<WasmOrigin>().unwrap();
            assert_eq!(
                format!("{}://{}:{}", origin.scheme, origin.host, origin.port),
                parsed
            );
        }
    }

    #[test]
    fn origins_reject_invalid_hosts() {
        let label = format!("https://{}.example.com", "a".repeat(64));
        let domain = format!("https://{}aa", "a.".repeat(126));
        for origin in [
            "https://api..example.com",
            "https://.example.com",
            "https://-api.example.com",
            "https://api-.example.com",
            &label,
            &domain,
            "https://api.example.com.",
            "https://api!.example.com",
            "https://api~.example.com",
            "https://api%2e.example.com",
            "https://bücher.example",
            "https://example.123",
            "http://1234",
            "http://1.2.3",
            "http://999.1.1.1",
            "http://01.1.1.1",
            "http://0x7f.0.0.1",
            "http://[::1",
            "http://::1",
            "http://[::1]]",
            "http://[::1]x",
            "http://[[::1]]",
            "http://[fe80::1%eth0]",
            "http://[fe80::1%25eth0]",
            "http://[v1.x]",
            "http://[not-an-ip]",
            "http://[127.0.0.1]",
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
        assert!(!allows("https://api.example.com:/"));
        assert!(!allows("https://api.example.com:abc/"));
        assert!(!allows("https://api.example.com:65979/"));
        assert!(!allows("https://api.example.com:[80]:443/"));
        assert!(allows("https://user:pw@api.example.com/"));
        assert!(!allows("https://example.com/"));
        assert!(!allows("https://api.example.com.evil.com/"));
        assert!(!allows("https://api.example.com./"));
        assert!(!allows("/v1"));
    }

    #[test]
    fn origins_deny_requests_without_canonical_ports() {
        let origin = "https://api.example.com".parse::<WasmOrigin>().unwrap();
        let allows = |uri: &str| origin.allows(&uri.parse().unwrap());

        assert!(!allows("https://api.example.com:0443/"));
        assert!(!allows("https://api.example.com:+443/"));
        assert!(!allows("https://api.example.com:443x/"));
        assert!(!allows("https://api.example.com:0/"));
        assert!(!allows("api.example.com:443"));
        assert!(!allows("https://user:pw@api.example.com:0443/"));
    }

    #[test]
    fn origins_reject_invalid_ports() {
        assert_eq!(
            "http://[::1]".parse::<WasmOrigin>().unwrap(),
            "http://[::1]:80".parse().unwrap(),
        );
        let origin = "http://[::1]:8080".parse::<WasmOrigin>().unwrap();
        assert!(origin.allows(&"http://[::1]:8080/".parse().unwrap()));
        assert!(!origin.allows(&"http://[::1]/".parse().unwrap()));
        assert!(!origin.allows(&"http://[::1]x:8080/".parse().unwrap()));
        assert!(!origin.allows(&"http://[::1]:08080/".parse().unwrap()));
        for origin in [
            "https://api.example.com:",
            "https://api.example.com:abc",
            "https://api.example.com:0",
            "https://api.example.com:00",
            "https://api.example.com:0443",
            "https://api.example.com:080",
            "https://api.example.com:65536",
            "https://api.example.com:99999",
            "https://api.example.com:100000",
            "https://api.example.com:+443",
            "https://api.example.com: 443",
            "https://api.example.com:8443x",
            "https://api.example.com:-1",
            "https://api.example.com:443:443",
            "https://api.example.com:[80]:443",
            "https://*.example.com:",
            "https://*.example.com:abc",
            "https://*.example.com:0443",
            "https://*.example.com:99999",
            "https://*.example.com:8443x",
            "https://*.example.com:-1",
            "https://*.example.com:[80]:443",
            "http://[::1]:",
            "http://[::1]:abc",
            "http://[::1]:0",
            "http://[::1]x:80",
        ] {
            let error = origin.parse::<WasmOrigin>().unwrap_err();
            assert_eq!(
                error.to_string(),
                format!("invalid HTTP origin \"{origin}\"")
            );
        }
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
            "https://*.example.1com",
            "https://*.example.com.",
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
    fn wildcard_origins_keep_ports() {
        let origin = "https://*.example.com:8443".parse::<WasmOrigin>().unwrap();
        let allows = |uri: &str| origin.allows(&uri.parse().unwrap());

        assert!(allows("https://api.example.com:8443/"));
        assert!(!allows("https://api.example.com/"));
        assert!(!allows("https://api.example.com:08443/"));
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
