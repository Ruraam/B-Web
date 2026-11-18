//! HTTP/HTTPS transport with a mandatory Shield admission gate.
//!
//! The underlying reqwest client is private. Callers cannot obtain it or send
//! requests around NetworkPolicy. Automatic redirects are disabled: every
//! redirect target is validated and checked by Shield before another request
//! is sent. TLS is provided by reqwest's rustls feature, not OpenSSL.

use bweb_shield::{Decision, ResourceType, Shield, ShieldError};

/// Errors returned while validating an HTTP or HTTPS URL.
#[derive(Debug)]
pub enum WebUrlError {
    Parse(url::ParseError),
    UnsupportedScheme(String),
}

impl fmt::Display for WebUrlError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Parse(error) => write!(f, "invalid web URL: {error}"),
            Self::UnsupportedScheme(scheme) => write!(f, "unsupported URL scheme: {scheme}"),
        }
    }
}

impl Error for WebUrlError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Parse(error) => Some(error),
            Self::UnsupportedScheme(_) => None,
        }
    }
}

impl From<url::ParseError> for WebUrlError {
    fn from(error: url::ParseError) -> Self {
        Self::Parse(error)
    }
}

/// Errors returned by the network admission policy.
#[derive(Debug)]
pub enum NetworkPolicyError {
    InvalidUrl(WebUrlError),
    Shield(ShieldError),
    /// The URL resolves to a local or internal address that the policy forbids.
    ForbiddenAddress(String),
    /// The hostname could not be resolved to any address.
    Resolve(String),
}

impl fmt::Display for NetworkPolicyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidUrl(error) => write!(f, "network request rejected: {error}"),
            Self::Shield(error) => write!(f, "network request could not be checked: {error}"),
            Self::ForbiddenAddress(address) => {
                write!(f, "network request to local or internal address rejected: {address}")
            }
            Self::Resolve(host) => write!(f, "could not resolve network host: {host}"),
        }
    }
}

impl Error for NetworkPolicyError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::InvalidUrl(error) => Some(error),
            Self::Shield(error) => Some(error),
            Self::ForbiddenAddress(_) | Self::Resolve(_) => None,
        }
    }
}

impl From<WebUrlError> for NetworkPolicyError {
    fn from(error: WebUrlError) -> Self {
        Self::InvalidUrl(error)
    }
}

impl From<ShieldError> for NetworkPolicyError {
    fn from(error: ShieldError) -> Self {
        Self::Shield(error)
    }
}

/// Parses an absolute HTTP or HTTPS URL.
pub fn parse_web_url(input: &str) -> Result<Url, WebUrlError> {
    let url = Url::parse(input)?;
    match url.scheme() {
        "http" | "https" => Ok(url),
        scheme => Err(WebUrlError::UnsupportedScheme(scheme.to_owned())),
    }
}

/// A hostname together with the addresses pinned for one request.
pub type ResolvedHost = (String, Vec<SocketAddr>);

/// How a policy treats addresses that are not globally routable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AddressPolicy {
    /// Reject loopback, private, link-local and similar internal addresses.
    /// This is the safe default for pages loaded from the web.
    DenyLocal,
    /// Permit loopback only, still rejecting private and link-local networks.
    /// Useful for local development and the test suite.
    AllowLoopback,
    /// Permit all addresses, including private networks. Use only for explicit
    /// local-network operation.
    AllowLocal,
}

/// Shield-backed policy that must authorize each outgoing web request.
pub struct NetworkPolicy {
    shield: Shield,
    address_policy: AddressPolicy,
}

impl NetworkPolicy {
    /// Creates a policy using the supplied compiled filter engine and the safe
    /// address policy that denies local and internal destinations.
    #[must_use]
    pub fn new(shield: Shield) -> Self {
        Self::with_address_policy(shield, AddressPolicy::DenyLocal)
    }

    /// Creates a policy with an explicit local-address policy.
    #[must_use]
    pub const fn with_address_policy(shield: Shield, address_policy: AddressPolicy) -> Self {
        Self { shield, address_policy }
    }

    /// Returns the configured address policy.
    #[must_use]
    pub const fn address_policy(&self) -> AddressPolicy {
        self.address_policy
    }

    /// Validates the destination address and, for hostnames, resolves it.
    /// The resolved addresses are returned so the transport can pin them and
    /// avoid a second DNS lookup (which could be a rebinding attempt).
    pub fn validate_and_resolve(
        &self,
        url: &Url,
    ) -> Result<Option<ResolvedHost>, NetworkPolicyError> {
        let host = url.host_str().ok_or_else(|| {
            NetworkPolicyError::InvalidUrl(WebUrlError::UnsupportedScheme("missing host".into()))
        })?;
        // `Url::host_str()` keeps the brackets of IPv6 literals.
        let host = host.trim_start_matches('[').trim_end_matches(']');
        if let Ok(ip) = host.parse::<IpAddr>() {
            if address_forbidden(ip, self.address_policy) {
                return Err(NetworkPolicyError::ForbiddenAddress(host.to_owned()));
            }
            return Ok(None);
        }

        let mut addresses: Vec<SocketAddr> = (host, 0)
            .to_socket_addrs()
            .map_err(|error| NetworkPolicyError::Resolve(format!("{host}: {error}")))?
            .collect();
        if addresses.is_empty() {
            return Err(NetworkPolicyError::Resolve(host.to_owned()));
        }
        addresses.dedup();
        if addresses.iter().any(|address| address_forbidden(address.ip(), self.address_policy)) {
            return Err(NetworkPolicyError::ForbiddenAddress(host.to_owned()));
        }
        Ok(Some((host.to_owned(), addresses)))
    }

    /// Checks a resource URL against Shield when the caller already validated URLs.
    pub fn authorize(
        &self,
        resource_url: &str,
        page_url: &str,
        resource_type: ResourceType,
        method: &str,
    ) -> Result<Decision, ShieldError> {
        self.shield.check(resource_url, page_url, resource_type, method)
    }

    /// Validates both URLs, checks Shield and resolves/validates the destination
    /// address in one pass, returning the addresses to pin for the request.
    pub fn authorize_and_resolve(
        &self,
        resource_url: &str,
        page_url: &str,
        resource_type: ResourceType,
        method: &str,
    ) -> Result<(Decision, Option<ResolvedHost>), NetworkPolicyError> {
        let resource_url = parse_web_url(resource_url)?;
        let page_url = parse_web_url(page_url)?;

        // Shield runs first so blocked URLs never incur a DNS lookup.
        let decision = self
            .authorize(resource_url.as_str(), page_url.as_str(), resource_type, method)
            .map_err(NetworkPolicyError::from)?;
        // A blocked request must never incur a DNS lookup.
        if matches!(decision, Decision::Block) {
            return Ok((decision, None));
        }
        let resolved = self.validate_and_resolve(&resource_url)?;
        Ok((decision, resolved))
    }

    /// Validates both URLs and checks the request against Shield.
    pub fn authorize_web_request(
        &self,
        resource_url: &str,
        page_url: &str,
        resource_type: ResourceType,
        method: &str,
    ) -> Result<Decision, NetworkPolicyError> {
        self.authorize_and_resolve(resource_url, page_url, resource_type, method)
            .map(|(decision, _)| decision)
    }
}

/// True when `ip` must not be contacted under the given policy. IPv4-mapped
/// IPv6 addresses are reduced to IPv4 so loopback/private forms cannot slip
/// through the IPv6 spelling.
fn address_forbidden(ip: IpAddr, policy: AddressPolicy) -> bool {
    match ip {
        IpAddr::V4(address) => {
            let octets = address.octets();
            if address.is_unspecified() || address.is_multicast() || octets[0] >= 240 {
                return true;
            }
            let private = octets[0] == 10
                || (octets[0] == 172 && (16..=31).contains(&octets[1]))
                || (octets[0] == 192 && octets[1] == 168);
            // 100.64/10 (carrier-grade NAT) and 169.254/16 (link-local, which
            // includes the cloud metadata endpoint) are internal as well.
            let internal = private
                || address.is_link_local()
                || (octets[0] == 100 && (64..=127).contains(&octets[1]));
            match policy {
                AddressPolicy::AllowLocal => false,
                AddressPolicy::AllowLoopback => !address.is_loopback(),
                AddressPolicy::DenyLocal => address.is_loopback() || internal,
            }
        }
        IpAddr::V6(address) => {
            // IPv4-mapped addresses must follow the IPv4 rules.
            if let Some(mapped) = address.to_ipv4_mapped() {
                return address_forbidden(IpAddr::V4(mapped), policy);
            }
            if address.is_unspecified() || address.is_multicast() {
                return true;
            }
            let first = address.segments()[0];
            // fc00::/7 unique-local and fe80::/10 link-local.
            let internal = (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80;
            match policy {
                AddressPolicy::AllowLocal => false,
                AddressPolicy::AllowLoopback => !address.is_loopback(),
                AddressPolicy::DenyLocal => address.is_loopback() || internal,
            }
        }
    }
}

use reqwest::{
    Method, StatusCode,
    blocking::{Client, Response},
    header::{
        AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE, COOKIE, HOST, HeaderMap, HeaderValue,
        LOCATION, PROXY_AUTHORIZATION,
    },
    redirect::Policy,
};
use std::{
    error::Error,
    fmt,
    net::{IpAddr, SocketAddr, ToSocketAddrs},
    time::Duration,
};
use url::Url;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_REDIRECTS: usize = 10;

/// Errors produced by the guarded HTTP transport.
#[derive(Debug)]
pub enum HttpTransportError {
    Client(reqwest::Error),
    Policy(NetworkPolicyError),
    Blocked { url: String },
    InvalidMethod(String),
    InvalidRedirectUrl(url::ParseError),
    RedirectWithoutLocation,
    TooManyRedirects { limit: usize },
}

impl fmt::Display for HttpTransportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Client(error) => write!(f, "HTTP transport failed: {error}"),
            Self::Policy(error) => write!(f, "network policy rejected request: {error}"),
            Self::Blocked { url } => write!(f, "request blocked by Shield: {url}"),
            Self::InvalidMethod(error) => write!(f, "invalid HTTP method: {error}"),
            Self::InvalidRedirectUrl(error) => write!(f, "invalid redirect URL: {error}"),
            Self::RedirectWithoutLocation => {
                f.write_str("redirect response has no Location header")
            }
            Self::TooManyRedirects { limit } => write!(f, "HTTP redirect limit exceeded ({limit})"),
        }
    }
}

impl Error for HttpTransportError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Client(error) => Some(error),
            Self::Policy(error) => Some(error),
            Self::InvalidRedirectUrl(error) => Some(error),
            Self::Blocked { .. }
            | Self::InvalidMethod(_)
            | Self::RedirectWithoutLocation
            | Self::TooManyRedirects { .. } => None,
        }
    }
}

impl From<NetworkPolicyError> for HttpTransportError {
    fn from(error: NetworkPolicyError) -> Self {
        Self::Policy(error)
    }
}

impl From<reqwest::Error> for HttpTransportError {
    fn from(error: reqwest::Error) -> Self {
        Self::Client(error)
    }
}

/// HTTP/HTTPS transport whose only request path is guarded by NetworkPolicy.
///
/// Both fields are private by design. The client cannot be extracted, replaced,
/// or used directly by downstream crates.
pub struct HttpTransport {
    client: Client,
    policy: NetworkPolicy,
}

/// Builds the guarded client, optionally pinning a hostname to the addresses
/// already validated by the policy so DNS cannot be resolved a second time.
fn build_client(resolved: Option<(&str, &[SocketAddr])>) -> Result<Client, reqwest::Error> {
    let mut builder = Client::builder()
        .https_only(false)
        .connect_timeout(CONNECT_TIMEOUT)
        .timeout(REQUEST_TIMEOUT)
        .redirect(Policy::none());
    if let Some((host, addresses)) = resolved {
        builder = builder.resolve_to_addrs(host, addresses);
    }
    builder.build()
}

impl HttpTransport {
    /// Creates a rustls-backed client and takes ownership of its network policy.
    ///
    /// Automatic redirects, which could bypass per-hop filtering, are disabled.
    pub fn new(policy: NetworkPolicy) -> Result<Self, HttpTransportError> {
        Ok(Self { client: build_client(None)?, policy })
    }

    /// Executes an HTTP(S) request only after a Shield check.
    ///
    /// Sec-GPC and DNT are forcibly set to 1 after caller headers are applied,
    /// so callers cannot disable either privacy signal. Every redirect hop is
    /// re-authorized. The response body is returned as a stream, not buffered.
    pub fn execute(
        &self,
        method: &str,
        resource_url: &str,
        page_url: &str,
        resource_type: ResourceType,
        headers: HeaderMap,
        body: Option<Vec<u8>>,
    ) -> Result<Response, HttpTransportError> {
        let mut method = Method::from_bytes(method.as_bytes())
            .map_err(|error| HttpTransportError::InvalidMethod(error.to_string()))?;
        let mut current_url = parse_web_url(resource_url)
            .map_err(NetworkPolicyError::from)
            .map_err(HttpTransportError::Policy)?;
        let page_url = parse_web_url(page_url)
            .map_err(NetworkPolicyError::from)
            .map_err(HttpTransportError::Policy)?;
        let mut headers = headers;
        // The URL, not caller-controlled headers, determines the HTTP authority.
        headers.remove(HOST);
        let mut body = body;

        for redirect_count in 0..=MAX_REDIRECTS {
            // One pass checks Shield, validates the address and returns the
            // resolved addresses, so the same result is pinned for the request.
            let (decision, resolved) = self
                .policy
                .authorize_and_resolve(
                    current_url.as_str(),
                    page_url.as_str(),
                    resource_type,
                    method.as_str(),
                )
                .map_err(HttpTransportError::Policy)?;
            if matches!(decision, Decision::Block) {
                return Err(HttpTransportError::Blocked { url: current_url.to_string() });
            }
            let pinned_client = match &resolved {
                Some((host, addresses)) => Some(build_client(Some((host, addresses)))?),
                None => None,
            };

            // Apply the mandatory privacy signals after caller-provided values.
            let request_headers = with_privacy_headers(headers.clone());
            let mut request = pinned_client
                .as_ref()
                .unwrap_or(&self.client)
                .request(method.clone(), current_url.clone())
                .headers(request_headers);

            if let Some(request_body) = body.as_ref() {
                request = request.body(request_body.clone());
            }

            let response = request.send()?;
            let status = response.status();
            if !is_redirect(status) {
                return Ok(response);
            }

            if redirect_count == MAX_REDIRECTS {
                return Err(HttpTransportError::TooManyRedirects { limit: MAX_REDIRECTS });
            }

            let location = response
                .headers()
                .get(LOCATION)
                .ok_or(HttpTransportError::RedirectWithoutLocation)?
                .to_str()
                .map_err(|_| HttpTransportError::RedirectWithoutLocation)?;
            let next_url =
                current_url.join(location).map_err(HttpTransportError::InvalidRedirectUrl)?;

            // Do not consume the redirect body or connect to the next hop yet.
            drop(response);

            if current_url.origin() != next_url.origin() {
                headers.remove(AUTHORIZATION);
                headers.remove(PROXY_AUTHORIZATION);
                headers.remove(COOKIE);
            }

            if should_switch_to_get(status, &method) {
                method = Method::GET;
                body = None;
                headers.remove(CONTENT_LENGTH);
                headers.remove(CONTENT_TYPE);
            }

            current_url = next_url;
        }

        Err(HttpTransportError::TooManyRedirects { limit: MAX_REDIRECTS })
    }
}

fn with_privacy_headers(mut headers: HeaderMap) -> HeaderMap {
    headers.insert("sec-gpc", HeaderValue::from_static("1"));
    headers.insert("dnt", HeaderValue::from_static("1"));
    headers
}

fn is_redirect(status: StatusCode) -> bool {
    matches!(
        status,
        StatusCode::MOVED_PERMANENTLY
            | StatusCode::FOUND
            | StatusCode::SEE_OTHER
            | StatusCode::TEMPORARY_REDIRECT
            | StatusCode::PERMANENT_REDIRECT
    )
}

fn should_switch_to_get(status: StatusCode, method: &Method) -> bool {
    (status == StatusCode::SEE_OTHER && method != Method::HEAD)
        || ((status == StatusCode::MOVED_PERMANENTLY || status == StatusCode::FOUND)
            && method == Method::POST)
}

#[cfg(test)]
mod tests {
    use super::{
        HttpTransport, HttpTransportError, is_redirect, should_switch_to_get, with_privacy_headers,
    };
    use crate::{AddressPolicy, NetworkPolicy, NetworkPolicyError, parse_web_url};
    use bweb_shield::{ResourceType, Shield};
    use reqwest::{
        Method, StatusCode,
        header::{HeaderMap, HeaderValue},
    };

    #[test]
    fn privacy_headers_override_caller_values() {
        let mut headers = HeaderMap::new();
        headers.insert("sec-gpc", HeaderValue::from_static("0"));
        headers.insert("dnt", HeaderValue::from_static("0"));

        let headers = with_privacy_headers(headers);
        assert_eq!(headers["sec-gpc"], "1");
        assert_eq!(headers["dnt"], "1");
    }

    #[test]
    fn redirect_statuses_are_explicitly_recognized() {
        assert!(is_redirect(StatusCode::FOUND));
        assert!(is_redirect(StatusCode::PERMANENT_REDIRECT));
        assert!(!is_redirect(StatusCode::BAD_REQUEST));
    }

    #[test]
    fn redirect_method_rules_match_http_semantics() {
        assert!(should_switch_to_get(StatusCode::SEE_OTHER, &Method::POST));
        assert!(!should_switch_to_get(StatusCode::SEE_OTHER, &Method::HEAD));
        assert!(should_switch_to_get(StatusCode::FOUND, &Method::POST));
        assert!(!should_switch_to_get(StatusCode::FOUND, &Method::GET));
        assert!(!should_switch_to_get(StatusCode::TEMPORARY_REDIRECT, &Method::POST));
    }

    #[test]
    fn blocked_request_is_rejected_before_network_io() {
        let policy = NetworkPolicy::new(Shield::from_rules(["||ads.example^"]));
        let transport = HttpTransport::new(policy).expect("rustls HTTP client");
        let error = transport
            .execute(
                "GET",
                "https://ads.example/pixel",
                "https://news.example/",
                ResourceType::Image,
                HeaderMap::new(),
                None,
            )
            .expect_err("Shield must block before connecting");

        assert!(matches!(error, HttpTransportError::Blocked { .. }));
    }

    #[test]
    fn transport_sends_gpc_and_dnt_on_the_wire() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let address = listener.local_addr().expect("local address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = [0_u8; 4096];
            let count = stream.read(&mut request).expect("read request headers");
            let request = String::from_utf8_lossy(&request[..count]).to_ascii_lowercase();
            assert!(request.contains("sec-gpc: 1"));
            assert!(request.contains("dnt: 1"));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .expect("write response");
        });

        let transport = HttpTransport::new(NetworkPolicy::with_address_policy(
            Shield::empty(),
            AddressPolicy::AllowLoopback,
        ))
        .expect("rustls HTTP client");
        let response = transport
            .execute(
                "GET",
                &format!("http://{address}/"),
                "http://page.example/",
                ResourceType::Document,
                HeaderMap::new(),
                None,
            )
            .expect("local request should pass");
        assert_eq!(response.status(), StatusCode::OK);
        server.join().expect("server thread");
    }

    #[test]
    fn redirect_target_is_rechecked_before_following() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local test server");
        let address = listener.local_addr().expect("local address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept first hop");
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request).expect("read first hop");
            stream.write_all(
                b"HTTP/1.1 302 Found\r\nLocation: https://ads.example/pixel\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
            ).expect("write redirect");
        });

        let policy = NetworkPolicy::with_address_policy(
            Shield::from_rules(["||ads.example^"]),
            AddressPolicy::AllowLoopback,
        );
        let transport = HttpTransport::new(policy).expect("rustls HTTP client");
        let error = transport
            .execute(
                "GET",
                &format!("http://{address}/redirect"),
                "http://page.example/",
                ResourceType::Image,
                HeaderMap::new(),
                None,
            )
            .expect_err("redirect destination must be blocked");
        assert!(matches!(error, HttpTransportError::Blocked { .. }));
        server.join().expect("server thread");
    }

    #[test]
    fn local_and_internal_addresses_are_rejected_by_default() {
        let policy = NetworkPolicy::new(Shield::empty());
        for url in [
            "http://127.0.0.1/",
            "http://127.1.2.3/",
            "http://10.0.0.1/",
            "http://172.16.0.1/",
            "http://192.168.1.1/",
            "http://169.254.169.254/latest/meta-data/",
            "http://[::1]/",
            "http://[::ffff:127.0.0.1]/",
            "http://[fd00::1]/",
            "http://[fe80::1]/",
        ] {
            let parsed = parse_web_url(url).expect("valid URL");
            let error = match policy.validate_and_resolve(&parsed) {
                Err(error) => error,
                Ok(_) => panic!("{url} must be rejected"),
            };
            assert!(
                matches!(error, NetworkPolicyError::ForbiddenAddress(_)),
                "{url} was rejected for the wrong reason: {error}"
            );
        }
        // A hostname resolving to loopback must not slip through either.
        let localhost = parse_web_url("http://localhost/").expect("URL");
        assert!(matches!(
            policy.validate_and_resolve(&localhost),
            Err(NetworkPolicyError::ForbiddenAddress(_))
        ));
    }

    #[test]
    fn address_policy_can_allow_loopback_without_allowing_private_networks() {
        let policy =
            NetworkPolicy::with_address_policy(Shield::empty(), AddressPolicy::AllowLoopback);
        let loopback = parse_web_url("http://127.0.0.1/").expect("URL");
        assert!(policy.validate_and_resolve(&loopback).expect("loopback allowed").is_none());

        for url in ["http://10.0.0.1/", "http://169.254.169.254/"] {
            let parsed = parse_web_url(url).expect("URL");
            assert!(matches!(
                policy.validate_and_resolve(&parsed),
                Err(NetworkPolicyError::ForbiddenAddress(_))
            ));
        }

        let permissive =
            NetworkPolicy::with_address_policy(Shield::empty(), AddressPolicy::AllowLocal);
        let private = parse_web_url("http://10.0.0.1/").expect("URL");
        assert!(permissive.validate_and_resolve(&private).expect("explicit opt-in").is_none());
    }

    #[test]
    fn redirect_to_an_internal_address_is_rejected_before_connecting() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        use std::thread;

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind local server");
        let address = listener.local_addr().expect("address");
        let server = thread::spawn(move || {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0_u8; 2048];
            let _ = stream.read(&mut request);
            stream
                .write_all(b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
                .expect("write redirect");
        });
        let policy =
            NetworkPolicy::with_address_policy(Shield::empty(), AddressPolicy::AllowLoopback);
        let transport = HttpTransport::new(policy).expect("client");
        let error = transport
            .execute(
                "GET",
                &format!("http://{address}/redirect"),
                "https://example.test/",
                ResourceType::Image,
                HeaderMap::new(),
                None,
            )
            .expect_err("internal redirect must be rejected");
        assert!(
            matches!(error, HttpTransportError::Policy(NetworkPolicyError::ForbiddenAddress(_))),
            "unexpected error: {error:?}"
        );
        server.join().expect("server thread");
    }
}
