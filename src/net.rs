//! Per-machine network settings (proxy, PAC, CA, client certificate) and the HTTP client
//! built from them. Stored in the gitignored `.state.toml`: proxies and cert paths differ
//! per machine, and the PFX password must never reach git.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use hyper_util::client::proxy::matcher::Matcher;
use serde::{Deserialize, Serialize};

use crate::http::error_chain;
use crate::model::{HttpVersion, Settings};

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug, Default)]
#[serde(rename_all = "lowercase")]
pub enum ProxyMode {
    /// OS settings; on Windows this includes the PAC script ("Use setup script", or found
    /// by WPAD when "Automatically detect settings" is on).
    #[default]
    System,
    None,
    Manual,
    Pac,
}

/// Each mode keeps its own fields so switching modes back and forth loses nothing.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Network {
    pub proxy: ProxyMode,
    pub proxy_url: String,
    pub no_proxy: String,
    pub pac_url: String,
    pub ca_file: String,
    pub client_cert: String,
    pub client_cert_password: String,
    /// Checked in order before `client_cert`; the first whose host matches wins.
    pub host_certs: Vec<HostCert>,
    pub insecure: bool,
    pub timeout_secs: u64,
    /// Sent unless a request sets its own. None is `USER_AGENT`, kept unset so it follows
    /// the version; empty sends none.
    pub user_agent: Option<String>,
    /// Empty still sends `*/*`: reqwest adds that to every request and can't be told not to.
    pub accept: String,
    /// Off sends no Accept-Encoding and leaves compressed bodies as they come.
    pub gzip: bool,
}

impl Default for Network {
    fn default() -> Self {
        Self {
            proxy: ProxyMode::System,
            proxy_url: String::new(),
            no_proxy: "localhost,127.0.0.1".into(),
            pac_url: String::new(),
            ca_file: String::new(),
            client_cert: String::new(),
            client_cert_password: String::new(),
            host_certs: Vec::new(),
            insecure: false,
            timeout_secs: 60,
            user_agent: None,
            accept: "*/*".into(),
            gzip: true,
        }
    }
}

/// A client certificate for one host, as Postman and Insomnia do: an API gateway and an
/// internal service rarely accept the same one.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug, Default)]
#[serde(default)]
pub struct HostCert {
    /// `api.example.com`, `*.example.com` (subdomains only), either with `:port`.
    pub host: String,
    /// Empty sends no certificate to this host, even when there is a default one.
    pub cert: String,
    pub password: String,
}

impl Network {
    /// The User-Agent a request without one gets; None sends none.
    pub fn user_agent(&self) -> Option<&str> {
        match &self.user_agent {
            None => Some(USER_AGENT),
            Some(agent) => Some(agent.trim()).filter(|a| !a.is_empty()),
        }
    }

    pub fn accept(&self) -> &str {
        match self.accept.trim() {
            "" => "*/*",
            accept => accept,
        }
    }

    /// The certificate path and password for a connection to `url`.
    pub fn cert_for(&self, url: &str) -> (&str, &str) {
        match self.host_cert(url) {
            Some(i) => (&self.host_certs[i].cert, &self.host_certs[i].password),
            None => (&self.client_cert, &self.client_cert_password),
        }
    }

    fn host_cert(&self, url: &str) -> Option<usize> {
        let url = reqwest::Url::parse(url.trim()).ok()?;
        let host = url.host_str()?.to_ascii_lowercase();
        let port = url.port_or_known_default();
        (self.host_certs.iter()).position(|c| host_matches(&c.host, &host, port))
    }
}

fn host_matches(pattern: &str, host: &str, port: Option<u16>) -> bool {
    let pattern = pattern.trim().to_ascii_lowercase();
    // `[::1]` ends in `]`, so its colons aren't a port.
    let (name, want) = match pattern.rsplit_once(':').filter(|_| !pattern.ends_with(']')) {
        Some((name, p)) => match p.parse::<u16>() {
            Ok(p) => (name, Some(p)),
            Err(_) => return false,
        },
        None => (pattern.as_str(), None),
    };
    if want.is_some_and(|w| Some(w) != port) {
        return false;
    }
    match name.strip_prefix("*.") {
        Some(base) => host
            .strip_suffix(base)
            .is_some_and(|sub| sub.ends_with('.')),
        None => !name.is_empty() && name == host,
    }
}

/// Sent unless a request sets its own, as Postman sends `PostmanRuntime/x.y`: without
/// one, some APIs (GitHub's) refuse the request.
pub const USER_AGENT: &str = concat!("apitool/", env!("CARGO_PKG_VERSION"));

/// gRPC needs HTTP/2 even over plain TCP (h2c), which reqwest only does with prior
/// knowledge, and that would break HTTP/1-only servers for everything else.
#[derive(Clone)]
pub struct Clients {
    pub http: reqwest::Client,
    pub grpc: reqwest::Client,
    /// Why the system's PAC script was skipped, to show next to the proxy mode.
    pub note: Option<String>,
    /// The same proxy choice, for connections reqwest doesn't make (MQTT).
    pub route: Route,
    /// What `pm.cookies` reads and writes: the jar these clients send from.
    pub jar: Arc<crate::cookies::Jar>,
    variants: Arc<Variants>,
}

#[derive(Clone, Default)]
pub enum Route {
    #[default]
    Direct,
    /// Manual settings, or the system's when it has no PAC script.
    Rules(Arc<hyper_util::client::proxy::matcher::Matcher>),
    Pac(Arc<Pac>),
}

impl Route {
    /// The proxy for a connection to `url` (http or https) and its Basic credentials;
    /// None is direct.
    pub fn proxy_for(
        &self,
        url: &reqwest::Url,
    ) -> Option<(reqwest::Url, Option<(String, String)>)> {
        match self {
            Route::Direct => None,
            Route::Pac(pac) => Some((reqwest::Url::parse(&pac.proxy_for(url)?).ok()?, None)),
            Route::Rules(rules) => {
                let hit = rules.intercept(&url.as_str().parse().ok()?)?;
                // The matcher only hands credentials out as a header value.
                let login = (hit.basic_auth())
                    .and_then(|v| v.to_str().ok()?.strip_prefix("Basic "))
                    .and_then(|b64| base64::engine::general_purpose::STANDARD.decode(b64).ok())
                    .and_then(|raw| {
                        let (user, password) = std::str::from_utf8(&raw).ok()?.split_once(':')?;
                        Some((user.to_owned(), password.to_owned()))
                    });
                Some((reqwest::Url::parse(&hit.uri().to_string()).ok()?, login))
            }
        }
    }
}

/// What request settings change about the connection itself (the timeout is set per
/// request instead). reqwest fixes these per client, so each combination in use gets
/// its own, built once. Settings that don't matter for a combination are left at their
/// defaults, so it shares a client with the others that differ only there.
#[derive(Clone, PartialEq, Eq, Hash)]
struct Variant {
    version: HttpVersion,
    /// None doesn't follow, or `http::follow` does.
    redirects: Option<u32>,
    /// A Referer on the redirects reqwest follows.
    referer: bool,
    insecure: bool,
    cookies: bool,
    /// Which of `host_certs` to present; None is the default certificate.
    cert: Option<usize>,
    no_tls12: bool,
    no_tls13: bool,
    /// The suites offered, by `ciphers` name; None offers all.
    ciphers: Option<Vec<&'static str>>,
    /// Accepts malformed response headers.
    lenient: bool,
    /// The capture proxy's: what an app sent goes on as it came, nothing added or decoded,
    /// and no timeout to cut a long download or stream.
    forward: bool,
    /// A name looked up as another host: a gRPC call's TLS server name (`server_alias`).
    alias: Option<(String, String)>,
}

impl From<&Settings> for Variant {
    fn from(s: &Settings) -> Self {
        let redirects = (s.follow_redirects && !s.follows_itself()).then_some(s.max_redirects);
        Self {
            version: s.http_version,
            redirects,
            referer: !(s.remove_referer && redirects.is_some()),
            insecure: !s.verify_tls,
            cookies: s.cookies,
            cert: None,
            no_tls12: s.disable_tls12,
            no_tls13: s.disable_tls13,
            // Unknown names are skipped; none known is Some(empty), refused when built.
            ciphers: (!s.ciphers.is_empty()).then(|| {
                s.ciphers
                    .iter()
                    .filter_map(|n| cipher(n))
                    .map(|c| c.0)
                    .collect()
            }),
            lenient: s.insecure_parser,
            forward: false,
            alias: None,
        }
    }
}

type Build = dyn Fn(Variant, &Arc<crate::cookies::Jar>, Option<Handshake>) -> Result<reqwest::Client, String>
    + Send
    + Sync;

struct Variants {
    /// Shared with the clients `with_jar` makes, which differ only in the jar. The third
    /// value makes a client for one NTLM handshake (see `ntlm_for`).
    build: Arc<Build>,
    built: Mutex<HashMap<Variant, reqwest::Client>>,
    /// For picking the certificate by host.
    net: Network,
}

impl Clients {
    /// What these clients were built from: the Timeline lists the headers it made them add.
    pub fn network(&self) -> &Network {
        &self.variants.net
    }

    /// The client for a request's settings and host; the shared one unless they differ
    /// from the defaults.
    pub fn for_settings(&self, s: &Settings, url: &str) -> Result<reqwest::Client, String> {
        let cert = self.variants.net.host_cert(url);
        self.variant(Variant { cert, ..s.into() })
    }

    /// The client for a WebSocket handshake to `url` (WebSocket, Socket.IO, GraphQL
    /// subscriptions) with the request's settings. The handshake is an HTTP/1.1 upgrade:
    /// offered h2 as well, a server such as Cloudflare picks it, and the upgrade then fails.
    pub fn websocket_for(&self, s: &Settings, url: &str) -> Result<reqwest::Client, String> {
        let cert = self.variants.net.host_cert(url);
        self.variant(Variant {
            version: HttpVersion::Http1,
            cert,
            ..s.into()
        })
    }

    /// A client for one NTLM handshake to `url`, never cached: NTLM signs in a connection,
    /// so the handshake must stay on one, and a shared pool could hand it another
    /// handshake's. HTTP/1.1, as HTTP/2 multiplexes requests from anywhere on one
    /// connection. The handshake is over when the second value drops.
    pub fn ntlm_for(
        &self,
        s: &Settings,
        url: &str,
    ) -> Result<(reqwest::Client, tokio::sync::watch::Sender<()>), String> {
        let (over, waiting) = tokio::sync::watch::channel(());
        let v = Variant {
            version: HttpVersion::Http1,
            cert: self.variants.net.host_cert(url),
            ..s.into()
        };
        let handshake = Handshake {
            opened: Default::default(),
            over: waiting,
        };
        Ok(((self.variants.build)(v, &self.jar, Some(handshake))?, over))
    }

    /// The client the capture proxy forwards a request for `url` with: the same proxy, CA
    /// file and client certificate, no redirects followed and no cookies of the app's.
    /// `version` as the app asked: h2c stays HTTP/2, a WebSocket's upgrade HTTP/1.1.
    pub fn forward_for(&self, url: &str, version: HttpVersion) -> Result<reqwest::Client, String> {
        let defaults = Variant::from(&Settings::default());
        self.variant(Variant {
            version,
            redirects: None,
            referer: false,
            cookies: false,
            cert: self.variants.net.host_cert(url),
            forward: true,
            ..defaults
        })
    }

    /// The HTTP/2 client for a gRPC call to `url` with the request's settings: given a TLS
    /// server name, one that finds it where the URL's host is.
    pub fn grpc_for(&self, s: &Settings, url: &str) -> Result<reqwest::Client, String> {
        let cert = self.variants.net.host_cert(url);
        self.variant(Variant {
            version: HttpVersion::Http2,
            cert,
            alias: server_alias(s, url),
            ..s.into()
        })
    }

    /// The same network setup sending from `jar`: a collection run that keeps its cookies
    /// apart from the app's (`runner::RunOptions`).
    pub fn with_jar(&self, jar: Arc<crate::cookies::Jar>) -> Result<Clients, String> {
        let build = &self.variants.build;
        Ok(Clients {
            http: build(Variant::from(&Settings::default()), &jar, None)?,
            grpc: build(grpc_variant(), &jar, None)?,
            note: self.note.clone(),
            route: self.route.clone(),
            jar,
            variants: Arc::new(Variants {
                build: build.clone(),
                built: Default::default(),
                net: self.variants.net.clone(),
            }),
        })
    }

    fn variant(&self, v: Variant) -> Result<reqwest::Client, String> {
        if v == Variant::from(&Settings::default()) {
            return Ok(self.http.clone());
        }
        if v == grpc_variant() {
            return Ok(self.grpc.clone());
        }
        if let Some(c) = self.variants.built.lock().unwrap().get(&v) {
            return Ok(c.clone());
        }
        let c = (self.variants.build)(v.clone(), &self.jar, None)?;
        self.variants.built.lock().unwrap().insert(v, c.clone());
        Ok(c)
    }
}

/// A gRPC call's TLS server name for `url`, and the URL's own host, which it's looked up
/// as: the call is addressed to the name (SNI, the certificate's check, :authority, see
/// `grpc::wire`) and connects where the URL says. Not over http://, which has no TLS, nor
/// when it's the URL's host already.
pub fn server_alias(s: &Settings, url: &str) -> Option<(String, String)> {
    let name = s.tls_server_name.trim().to_ascii_lowercase();
    let url = reqwest::Url::parse(url.trim()).ok()?;
    let host = url
        .host_str()?
        .trim_matches(['[', ']'])
        .to_ascii_lowercase();
    (!name.is_empty() && url.scheme() == "https" && name != host).then_some((name, host))
}

fn grpc_variant() -> Variant {
    Variant {
        version: HttpVersion::Http2,
        ..Variant::from(&Settings::default())
    }
}

/// With a fresh cookie jar for this client's lifetime (a CLI run, an MCP session).
pub async fn build_client(net: Network) -> Result<Clients, String> {
    build_client_with_jar(net, Default::default()).await
}

/// With a jar that outlives the client: the GUI keeps cookies across network changes.
pub async fn build_client_with_jar(
    net: Network,
    jar: Arc<crate::cookies::Jar>,
) -> Result<Clients, String> {
    // reqwest is built with `rustls-no-provider` (ring cross-compiles to Windows with just
    // clang; aws-lc-rs needs cmake/nasm). Installing is idempotent, and doing it here covers
    // every client, including the PAC fetcher below.
    install_provider();

    // Any explicit `.proxy()` turns off reqwest's own system-proxy lookup.
    let mut note = None;
    let (proxy, route) = match net.proxy {
        ProxyMode::System => {
            let (pac, skipped) = system_proxy(system_pac_url().await?).await;
            note = skipped;
            match pac {
                Some(pac) => (Some(pac_proxy(&pac)), Route::Pac(pac)),
                // Read once, here, for every client these settings build: reqwest reads
                // them again for each client it builds, and one built while the capture
                // is the system's proxy would forward the capture's requests into itself.
                None => {
                    let route = Route::Rules(Arc::new(Matcher::from_system()));
                    (Some(route_proxy(&route)), route)
                }
            }
        }
        ProxyMode::None => (None, Route::Direct),
        ProxyMode::Manual => {
            let url = net.proxy_url.trim();
            // reqwest's own word for it is "relative URL without a base".
            if url.is_empty() {
                return Err(
                    "Manual proxy without a URL: type one, such as http://proxy:8080".into(),
                );
            }
            let proxy =
                reqwest::Proxy::all(url).map_err(|e| format!("proxy URL: {}", error_chain(&e)))?;
            let rules = Matcher::builder()
                .all(url.to_owned())
                .no(net.no_proxy.clone())
                .build();
            (
                Some(proxy.no_proxy(reqwest::NoProxy::from_string(&net.no_proxy))),
                Route::Rules(Arc::new(rules)),
            )
        }
        ProxyMode::Pac => {
            let pac = load_pac(net.pac_url.trim()).await?;
            (Some(pac_proxy(&pac)), Route::Pac(pac))
        }
    };
    let how = match &route {
        Route::Direct => "direct",
        Route::Rules(_) => "proxy rules",
        Route::Pac(_) => "PAC script",
    };
    let host_certs = net.host_certs.len();
    log::info!(
        "network: {:?} proxy ({how}), CA file {:?}, client certificate {:?}, {host_certs} host certificates, verify TLS {}, timeout {} s",
        net.proxy,
        net.ca_file.trim(),
        net.client_cert.trim(),
        !net.insecure,
        net.timeout_secs
    );

    let ca = net.ca_file.trim();
    let mut certs = Vec::new();
    if !ca.is_empty() {
        let pem = std::fs::read(ca).map_err(|e| format!("CA file {ca}: {e}"))?;
        certs = reqwest::Certificate::from_pem_bundle(&pem)
            .map_err(|e| format!("CA file {ca}: {e}"))?;
        if certs.is_empty() {
            return Err(format!("CA file {ca}: no PEM certificates found"));
        }
    }

    let load = |cert: &str, password: &str| match cert.trim() {
        "" => Ok(None),
        cert => load_identity(Path::new(cert), password).map(Some),
    };
    let identity = load(&net.client_cert, &net.client_cert_password)?;
    // Loaded up front so a wrong path or password shows now, not on some later request.
    let host_ids = (net.host_certs.iter())
        .map(|c| load(&c.cert, &c.password).map_err(|e| format!("{}: {e}", c.host.trim())))
        .collect::<Result<Vec<_>, _>>()?;
    let picking = net.clone();

    let build = move |v: Variant, jar: &Arc<crate::cookies::Jar>, ntlm: Option<Handshake>| {
        // Replaces reqwest's own `*/*`; a request's Accept still wins over it.
        let accept = reqwest::header::HeaderValue::from_str(net.accept())
            .map_err(|_| format!("Accept {:?}: not a valid header value", net.accept()))?;
        let wait = Duration::from_secs(net.timeout_secs.max(1));
        let mut b = match v.forward {
            true => reqwest::Client::builder().connect_timeout(wait).no_gzip(),
            false => reqwest::Client::builder()
                .default_headers([(reqwest::header::ACCEPT, accept)].into_iter().collect())
                .timeout(wait),
        }
        .tls_danger_accept_invalid_certs(net.insecure || v.insecure)
        .redirect(match v.redirects {
            // `Policy::limited`, plus noting each hop for the Timeline.
            Some(n) => reqwest::redirect::Policy::custom(move |attempt| {
                // The first of `previous` is the URL asked for, not a redirect.
                if attempt.previous().len() > n as usize {
                    return attempt.error("too many redirects");
                }
                let hop = (attempt.status().as_u16(), attempt.url().to_string());
                crate::http::trace(|t| t.hops.push(hop));
                attempt.follow()
            }),
            None => reqwest::redirect::Policy::none(),
        });
        if let Some(agent) = net.user_agent().filter(|_| !v.forward) {
            b = b.user_agent(agent);
        }
        if !net.gzip {
            b = b.no_gzip();
        }
        if v.cookies {
            b = b.cookie_provider(jar.clone());
        }
        if net.proxy == ProxyMode::None {
            b = b.no_proxy();
        }
        if let Some(p) = &proxy {
            b = b.proxy(p.clone());
        }
        if !certs.is_empty() {
            // Merge keeps the OS trust store; the corporate root is added on top.
            b = b.tls_certs_merge(certs.clone());
        }
        let id = match v.cert {
            Some(i) => &host_ids[i],
            None => &identity,
        };
        if let Some(id) = id {
            b = b.identity(id.clone());
        }
        b = b.connector_layer(tower_layer::layer_fn(TimeConnect));
        if let Some(h) = ntlm {
            b = b.connector_layer(tower_layer::layer_fn(move |inner| OneConnection {
                inner,
                handshake: h.clone(),
            }));
        }
        b = b.dns_resolver(TimeDns {
            alias: v.alias.clone(),
        });
        b = match v.version {
            HttpVersion::Auto => b,
            HttpVersion::Http1 => b.http1_only(),
            // Over TLS this offers only h2; over plain TCP it is h2c.
            HttpVersion::Http2 => b.http2_prior_knowledge(),
        };
        b = b.referer(v.referer);
        if v.lenient {
            // What Node's insecureHTTPParser lets through, which Postman's setting turns on.
            b = b
                .http1_ignore_invalid_headers_in_responses(true)
                .http1_allow_spaces_after_header_name_in_responses(true)
                .http1_allow_obsolete_multiline_headers_in_responses(true);
        }
        if v.no_tls12 || v.no_tls13 || v.ciphers.is_some() {
            b = b.tls_backend_preconfigured(custom_tls(&net, &v)?);
        }
        b.build().map_err(|e| error_chain(&e))
    };
    Ok(Clients {
        http: build(Variant::from(&Settings::default()), &jar, None)?,
        grpc: build(grpc_variant(), &jar, None)?,
        note,
        route,
        jar,
        variants: Arc::new(Variants {
            build: Arc::new(build),
            built: Default::default(),
            net: picking,
        }),
    })
}

/// A request's TLS versions and cipher suites. reqwest takes these only as a whole rustls
/// config (its own is built from the process-wide provider), so this one is built as
/// reqwest builds its own: the trust store and CA file, the client certificate, ALPN for
/// the HTTP version, and `HelloAt` for the TLS time.
fn custom_tls(net: &Network, v: &Variant) -> Result<rustls::ClientConfig, String> {
    let suites = match &v.ciphers {
        None => rustls::crypto::ring::ALL_CIPHER_SUITES.to_vec(),
        Some(names) => names
            .iter()
            .filter_map(|n| cipher(n))
            .map(|c| c.1)
            .collect(),
    };
    if suites.is_empty() {
        return Err("none of the request's cipher suites is one apitool has".into());
    }
    let versions: Vec<_> = [
        (&rustls::version::TLS12, v.no_tls12),
        (&rustls::version::TLS13, v.no_tls13),
    ]
    .into_iter()
    .filter_map(|(version, off)| (!off).then_some(version))
    .collect();
    if versions.is_empty() {
        return Err(
            "the request disables both TLS 1.2 and 1.3, the only versions apitool has".into(),
        );
    }
    let cert = match v.cert {
        Some(i) => (
            net.host_certs[i].cert.as_str(),
            net.host_certs[i].password.as_str(),
        ),
        None => (net.client_cert.as_str(), net.client_cert_password.as_str()),
    };
    let provider = rustls::crypto::CryptoProvider {
        cipher_suites: suites,
        ..provider()
    };
    let insecure = net.insecure || v.insecure;
    let mut tls = crate::mqtt::tls_config(net, cert, insecure, provider, &versions)?;
    tls.alpn_protocols = match v.version {
        HttpVersion::Auto => vec![b"h2".to_vec(), b"http/1.1".to_vec()],
        HttpVersion::Http1 => vec![b"http/1.1".to_vec()],
        HttpVersion::Http2 => vec![b"h2".to_vec()],
    };
    Ok(tls)
}

/// OpenSSL's names for the suites ring has, which Postman (`tlsCipherSelection`) and
/// curl (`--ciphers`) take.
const CIPHER_NAMES: &[(&str, rustls::CipherSuite)] = {
    use rustls::CipherSuite as C;
    &[
        ("TLS_AES_256_GCM_SHA384", C::TLS13_AES_256_GCM_SHA384),
        ("TLS_AES_128_GCM_SHA256", C::TLS13_AES_128_GCM_SHA256),
        (
            "TLS_CHACHA20_POLY1305_SHA256",
            C::TLS13_CHACHA20_POLY1305_SHA256,
        ),
        (
            "ECDHE-ECDSA-AES256-GCM-SHA384",
            C::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        ),
        (
            "ECDHE-ECDSA-AES128-GCM-SHA256",
            C::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        ),
        (
            "ECDHE-ECDSA-CHACHA20-POLY1305",
            C::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        ),
        (
            "ECDHE-RSA-AES256-GCM-SHA384",
            C::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        ),
        (
            "ECDHE-RSA-AES128-GCM-SHA256",
            C::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        ),
        (
            "ECDHE-RSA-CHACHA20-POLY1305",
            C::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
        ),
    ]
};

/// The cipher suites a request can pick: ring's, in rustls's order of preference.
pub fn ciphers() -> impl Iterator<Item = (&'static str, rustls::SupportedCipherSuite)> {
    rustls::crypto::ring::ALL_CIPHER_SUITES
        .iter()
        .filter_map(|s| {
            let (name, _) = CIPHER_NAMES.iter().find(|(_, id)| *id == s.suite())?;
            Some((*name, *s))
        })
}

/// A suite by OpenSSL's name, or IANA's as rustls spells it.
pub fn cipher(name: &str) -> Option<(&'static str, rustls::SupportedCipherSuite)> {
    let name = name.trim();
    ciphers().find(|(openssl, s)| {
        openssl.eq_ignore_ascii_case(name)
            || (s.suite().as_str()).is_some_and(|iana| iana.eq_ignore_ascii_case(name))
    })
}

/// The system's PAC script, or direct when it can't be used, as browsers do: WPAD often
/// finds an intranet web page instead of a script. The second value says why it was skipped.
async fn system_proxy(pac: Option<String>) -> (Option<Arc<Pac>>, Option<String>) {
    let Some(url) = pac else {
        return (None, None);
    };
    match load_pac(&url).await {
        Ok(pac) => (Some(pac), None),
        Err(e) => {
            log::warn!("system proxy script {url} skipped, going direct: {e}");
            (
                None,
                Some(crate::i18n::tf(
                    "The system's proxy script {} can't be used, so requests go out directly \
                     (as in a browser).\n{}",
                    &[&url, &e],
                )),
            )
        }
    }
}

/// Accepts a PEM file (key + certificate chain) or a PFX/P12 bundle.
fn load_identity(path: &Path, password: &str) -> Result<reqwest::Identity, String> {
    let pem = identity_pem(path, password)?;
    reqwest::Identity::from_pem(&pem)
        .map_err(|e| format!("client certificate {}: {}", path.display(), error_chain(&e)))
}

/// The client certificate file as PEM (key and chain), a PFX/P12 converted: for reqwest
/// and for MQTT's own TLS.
pub fn identity_pem(path: &Path, password: &str) -> Result<Vec<u8>, String> {
    let shown = path.display();
    let data = std::fs::read(path).map_err(|e| format!("client certificate {shown}: {e}"))?;
    let is_pkcs12 = path
        .extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| x.eq_ignore_ascii_case("pfx") || x.eq_ignore_ascii_case("p12"));
    match is_pkcs12 {
        true => pkcs12_to_pem(&data, password),
        false => Ok(data),
    }
}

/// reqwest+rustls only takes PEM identities, while corporate client certs usually ship as PFX.
fn pkcs12_to_pem(der: &[u8], password: &str) -> Result<Vec<u8>, String> {
    let store = p12_keystore::KeyStore::from_pkcs12(der, password, Default::default())
        .map_err(|e| format!("PFX: {e} (wrong password?)"))?;
    let (_, chain) = store
        .private_key_chain()
        .ok_or("PFX contains no private key")?;
    let mut pem = String::new();
    push_pem(&mut pem, "PRIVATE KEY", chain.key().as_der());
    for cert in chain.certs() {
        push_pem(&mut pem, "CERTIFICATE", cert.as_der());
    }
    Ok(pem.into_bytes())
}

fn push_pem(out: &mut String, label: &str, der: &[u8]) {
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    out.push_str(&format!("-----BEGIN {label}-----\n"));
    for line in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(line).expect("base64 is ascii"));
        out.push('\n');
    }
    out.push_str(&format!("-----END {label}-----\n"));
}

/// Windows' "Automatic configuration" settings: the PAC script address, and whether
/// "Automatically detect settings" (WPAD) is on. Cheap: no network traffic.
#[cfg(windows)]
pub fn system_auto_config() -> (Option<String>, bool) {
    use windows_sys::Win32::Networking::WinHttp::*;
    let mut c = WINHTTP_CURRENT_USER_IE_PROXY_CONFIG::default();
    // SAFETY: on success the strings are WinHTTP allocations we own (taken by `take_wide`).
    if unsafe { WinHttpGetIEProxyConfigForCurrentUser(&mut c) } == 0 {
        // No settings stored yet: Windows' default is auto-detect on (Chromium does the same).
        return (None, true);
    }
    let pac = unsafe {
        take_wide(c.lpszProxy);
        take_wide(c.lpszProxyBypass);
        take_wide(c.lpszAutoConfigUrl)
    };
    (pac.filter(|u| !u.trim().is_empty()), c.fAutoDetect != 0)
}

/// WPAD: the PAC address from DHCP, else DNS (`http://wpad.<domain>/wpad.dat`). Blocks
/// for seconds when nothing answers, which is why browsers only try it when asked to.
#[cfg(windows)]
fn detect_wpad() -> Option<String> {
    use windows_sys::Win32::Networking::WinHttp::*;
    let mut url = std::ptr::null_mut();
    let flags = WINHTTP_AUTO_DETECT_TYPE_DHCP | WINHTTP_AUTO_DETECT_TYPE_DNS_A;
    if unsafe { WinHttpDetectAutoProxyConfigUrl(flags, &mut url) } == 0 {
        return None;
    }
    // SAFETY: on success `url` is a WinHTTP allocation we own.
    unsafe { take_wide(url) }
}

/// Copies a NUL-terminated WinHTTP string and frees it. Null is None.
#[cfg(windows)]
pub(crate) unsafe fn take_wide(p: windows_sys::core::PWSTR) -> Option<String> {
    if p.is_null() {
        return None;
    }
    unsafe {
        let len = (0..).take_while(|&i| *p.add(i) != 0).count();
        let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, len));
        windows_sys::Win32::Foundation::GlobalFree(p.cast());
        Some(s)
    }
}

// ponytail: macOS system PAC isn't read (dev machine only); use PAC mode explicitly there.
#[cfg(not(windows))]
pub fn system_auto_config() -> (Option<String>, bool) {
    (None, false)
}

#[cfg(not(windows))]
fn detect_wpad() -> Option<String> {
    None
}

/// The PAC script System mode uses: the configured one, else WPAD discovery if it's on.
async fn system_pac_url() -> Result<Option<String>, String> {
    let (pac, wpad) = system_auto_config();
    log::debug!("system proxy: PAC script {pac:?}, automatically detect {wpad}");
    match (pac, wpad) {
        (Some(url), _) => Ok(Some(url)),
        (None, true) => {
            let found = tokio::task::spawn_blocking(detect_wpad)
                .await
                .map_err(|e| format!("WPAD: {e}"))?;
            log::info!("WPAD found {found:?}");
            Ok(found)
        }
        (None, false) => Ok(None),
    }
}

async fn load_pac(location: &str) -> Result<Arc<Pac>, String> {
    Ok(Arc::new(Pac::new(&fetch_pac(location).await?)?))
}

fn pac_proxy(pac: &Arc<Pac>) -> reqwest::Proxy {
    let pac = pac.clone();
    reqwest::Proxy::custom(move |url| pac.proxy_for(url))
}

/// The login goes back into the URL, where reqwest takes it from.
fn route_proxy(route: &Route) -> reqwest::Proxy {
    let route = route.clone();
    reqwest::Proxy::custom(move |url| {
        let (mut proxy, login) = route.proxy_for(url)?;
        if let Some((user, password)) = login {
            let _ = proxy.set_username(&user);
            let _ = proxy.set_password(Some(&password));
        }
        Some(proxy)
    })
}

async fn fetch_pac(location: &str) -> Result<String, String> {
    let err = |e: &dyn std::error::Error| format!("PAC {location}: {}", error_chain(e));
    if location.starts_with("http://") || location.starts_with("https://") {
        // The PAC file itself is always fetched directly: it's what tells us about the proxy.
        let client = reqwest::Client::builder()
            .no_proxy()
            .timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| err(&e))?;
        let resp = client
            .get(location)
            .send()
            .await
            .and_then(|r| r.error_for_status())
            .map_err(|e| err(&e))?;
        let text = resp.text().await.map_err(|e| err(&e))?;
        log::debug!("PAC script from {location}: {} B", text.len());
        return Ok(text);
    }
    let path = match reqwest::Url::parse(location) {
        Ok(url) if url.scheme() == "file" => url
            .to_file_path()
            .map_err(|()| format!("PAC {location}: bad file URL"))?,
        _ => location.into(),
    };
    std::fs::read_to_string(&path).map_err(|e| err(&e))
}

/// The standard PAC helper functions. `dnsResolve` and `myIpAddress` come from Rust.
const PAC_HELPERS: &str = r#"
function isPlainHostName(host) { return host.indexOf('.') < 0; }
function dnsDomainIs(host, domain) {
  return host.length >= domain.length && host.substring(host.length - domain.length) === domain;
}
function localHostOrDomainIs(host, hostdom) { return host === hostdom || hostdom.lastIndexOf(host + '.', 0) === 0; }
function dnsDomainLevels(host) { return host.split('.').length - 1; }
function isResolvable(host) { return dnsResolve(host) !== null; }
function shExpMatch(str, pat) {
  var re = pat.replace(/[.+^${}()|[\]\\]/g, '\\$&').replace(/\*/g, '.*').replace(/\?/g, '.');
  return new RegExp('^' + re + '$').test(str);
}
function __ip4(ip) { var p = ip.split('.'); return ((p[0] << 24) | (p[1] << 16) | (p[2] << 8) | p[3]) >>> 0; }
function isInNet(host, pattern, mask) {
  var ip = /^\d+\.\d+\.\d+\.\d+$/.test(host) ? host : dnsResolve(host);
  if (!ip) return false;
  return ((__ip4(ip) & __ip4(mask)) >>> 0) === ((__ip4(pattern) & __ip4(mask)) >>> 0);
}
// ponytail: time-based rules are treated as always matching; implement if a real PAC needs them.
function weekdayRange() { return true; }
function dateRange() { return true; }
function timeRange() { return true; }
"#;

pub struct Pac {
    // Field order matters: the context must drop before its runtime.
    ctx: Mutex<rquickjs::Context>,
    _rt: rquickjs::Runtime,
    cache: Mutex<HashMap<String, Option<String>>>,
}

impl Pac {
    pub fn new(script: &str) -> Result<Self, String> {
        let js = |e: rquickjs::Error| format!("PAC script: {e}");
        let rt = rquickjs::Runtime::new().map_err(js)?;
        rt.set_memory_limit(16 << 20);
        let ctx = rquickjs::Context::full(&rt).map_err(js)?;
        ctx.with(|ctx| -> Result<(), String> {
            let g = ctx.globals();
            let caught = |e: rquickjs::Error| exception(&ctx, e);
            g.set(
                "dnsResolve",
                rquickjs::Function::new(ctx.clone(), dns_resolve),
            )
            .map_err(caught)?;
            g.set(
                "myIpAddress",
                rquickjs::Function::new(ctx.clone(), my_ip_address),
            )
            .map_err(caught)?;
            ctx.eval::<(), _>(PAC_HELPERS).map_err(caught)?;
            ctx.eval::<(), _>(script).map_err(caught)?;
            g.get::<_, rquickjs::Function>("FindProxyForURL")
                .map(drop)
                .map_err(|_| "PAC script: FindProxyForURL is not defined".to_owned())
        })?;
        Ok(Self {
            ctx: Mutex::new(ctx),
            _rt: rt,
            cache: Mutex::new(HashMap::new()),
        })
    }

    fn find(&self, url: &str, host: &str) -> Result<String, String> {
        let ctx = self.ctx.lock().expect("PAC lock");
        ctx.with(|ctx| {
            let f: rquickjs::Function = ctx
                .globals()
                .get("FindProxyForURL")
                .map_err(|e| exception(&ctx, e))?;
            f.call::<_, String>((url, host))
                .map_err(|e| exception(&ctx, e))
        })
    }

    /// Proxy URL for a request, or None for DIRECT.
    // ponytail: cached per scheme+host and PAC errors mean DIRECT; path-based rules and
    // proxy failover ("PROXY a; PROXY b") would need per-URL evaluation and retries.
    pub fn proxy_for(&self, url: &reqwest::Url) -> Option<String> {
        let host = url.host_str()?;
        let key = format!("{}://{host}", url.scheme());
        if let Some(hit) = self.cache.lock().expect("PAC cache").get(&key) {
            return hit.clone();
        }
        // An error is DIRECT, as in a browser, but said here: nothing else shows it.
        let proxy = match self.find(url.as_str(), host) {
            Ok(r) => {
                log::debug!("PAC for {key}: {r}");
                first_proxy(&r)
            }
            Err(e) => {
                log::warn!("PAC for {key}, going direct: {e}");
                None
            }
        };
        self.cache
            .lock()
            .expect("PAC cache")
            .insert(key, proxy.clone());
        proxy
    }
}

fn exception(ctx: &rquickjs::Ctx<'_>, e: rquickjs::Error) -> String {
    if e.is_exception() {
        let value = ctx.catch();
        let message = value
            .as_exception()
            .and_then(|x| x.message())
            .or_else(|| value.as_string().and_then(|s| s.to_string().ok()));
        if let Some(m) = message {
            return format!("PAC script: {m}");
        }
    }
    format!("PAC script: {e}")
}

/// First entry reqwest can use from a PAC result like "PROXY a:8080; SOCKS b:1080; DIRECT".
fn first_proxy(result: &str) -> Option<String> {
    for entry in result.split(';') {
        let mut parts = entry.split_whitespace();
        let kind = parts.next().unwrap_or("").to_ascii_uppercase();
        match (kind.as_str(), parts.next()) {
            ("DIRECT", _) => return None,
            ("PROXY" | "HTTP", Some(addr)) => return Some(format!("http://{addr}")),
            ("HTTPS", Some(addr)) => return Some(format!("https://{addr}")),
            // As browsers read PAC: SOCKS is version 4. Names are resolved by the proxy
            // (4a, 5h), which is what an intranet behind one needs.
            ("SOCKS" | "SOCKS4", Some(addr)) => return Some(format!("socks4a://{addr}")),
            ("SOCKS5", Some(addr)) => return Some(format!("socks5h://{addr}")),
            _ => {}
        }
    }
    None
}

/// Times each new connection for the response's time hover. It runs inside the send
/// (unless hyper finishes it in the background for a later request), where TRACE is.
#[derive(Clone)]
struct TimeConnect<S>(S);

impl<R, S> tower_service::Service<R> for TimeConnect<S>
where
    S: tower_service::Service<R>,
    S::Response: hyper_util::client::legacy::connect::Connection,
    S::Future: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), S::Error>> {
        self.0.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        let started = Instant::now();
        let connecting = self.0.call(req);
        Box::pin(async move {
            crate::http::trace(|t| (t.hello, t.dns) = (None, None));
            let conn = connecting.await;
            let done = Instant::now();
            let Ok(c) = &conn else { return conn };
            let mut info = http::Extensions::new();
            hyper_util::client::legacy::connect::Connection::connected(c).get_extras(&mut info);
            let info = info.get::<hyper_util::client::legacy::connect::HttpInfo>();
            crate::http::trace(|t| {
                t.opened.push(crate::http::Opened {
                    local: info.map(|i| i.local_addr()),
                    hop: t.hops.len(),
                    connect: done - started,
                    dns: t.dns.take(),
                    // Through an https:// proxy the first hello is the proxy's, so TLS also
                    // holds the CONNECT and the server's own handshake.
                    tls: t.hello.take().map(|hello| done - hello),
                })
            });
            conn
        })
    }
}

/// One NTLM handshake's connections; see `OneConnection`.
#[derive(Clone)]
struct Handshake {
    opened: Arc<std::sync::atomic::AtomicBool>,
    /// Closes when `ntlm_for`'s sender drops, at the handshake's end.
    over: tokio::sync::watch::Receiver<()>,
}

/// Keeps an NTLM handshake on one connection. hyper's pool doesn't: when the connection
/// that took the negotiate message isn't back in the pool yet (it goes back from a
/// spawned task), the next request races a new connect against waiting for it, and the
/// new one can win. So a connect after the first waits: for the handshake's end, which
/// means hyper got the first connection back and only finishes this one in the
/// background (it then fails, unused); or for `NTLM_WAIT`, which means the server closed
/// the first, and the request goes out on a new one (the server answers 401).
#[derive(Clone)]
struct OneConnection<S> {
    inner: S,
    handshake: Handshake,
}

const NTLM_WAIT: Duration = Duration::from_secs(2);

impl<R, S> tower_service::Service<R> for OneConnection<S>
where
    S: tower_service::Service<R>,
    S::Future: Send + 'static,
    S::Error: From<&'static str>,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future =
        std::pin::Pin<Box<dyn std::future::Future<Output = Result<S::Response, S::Error>> + Send>>;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), S::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: R) -> Self::Future {
        use std::sync::atomic::Ordering::SeqCst;
        // Not started until polled: reqwest's connector is an async block.
        let connecting = self.inner.call(req);
        let first = !self.handshake.opened.swap(true, SeqCst);
        let mut over = self.handshake.over.clone();
        Box::pin(async move {
            if !first
                && tokio::time::timeout(NTLM_WAIT, over.changed())
                    .await
                    .is_ok()
            {
                return Err("NTLM handshake over; connection not needed".into());
            }
            connecting.await
        })
    }
}

/// ring, with `HelloAt` drawing its random numbers. Installing is idempotent: the first
/// install wins, so every place that installs one goes through here.
pub fn install_provider() {
    let _ = provider().install_default();
}

/// What `install_provider` installs; a request with its own cipher suites starts from it.
fn provider() -> rustls::crypto::CryptoProvider {
    rustls::crypto::CryptoProvider {
        secure_random: &HelloAt,
        ..rustls::crypto::ring::default_provider()
    }
}

/// Marks when a TLS handshake starts, for the time hover's TCP/TLS split. reqwest's
/// connector does TCP, a proxy's CONNECT and TLS in one call, but rustls draws the
/// ClientHello's random first thing in a handshake, and draws it from the provider. Later
/// draws in the same handshake keep the first mark.
#[derive(Debug)]
struct HelloAt;

impl rustls::crypto::SecureRandom for HelloAt {
    fn fill(&self, buf: &mut [u8]) -> Result<(), rustls::crypto::GetRandomFailed> {
        crate::http::trace(|t| {
            t.hello.get_or_insert_with(Instant::now);
        });
        ring::rand::SecureRandom::fill(&ring::rand::SystemRandom::new(), buf)
            .map_err(|_| rustls::crypto::GetRandomFailed)
    }
}

/// The system resolver (getaddrinfo, as reqwest's own), timed like `TimeConnect`, which it
/// runs inside.
struct TimeDns {
    /// This name is looked up as that host.
    alias: Option<(String, String)>,
}

impl reqwest::dns::Resolve for TimeDns {
    fn resolve(&self, name: reqwest::dns::Name) -> reqwest::dns::Resolving {
        let host = match &self.alias {
            Some((alias, host)) if name.as_str().eq_ignore_ascii_case(alias) => host.clone(),
            _ => name.as_str().to_owned(),
        };
        Box::pin(async move {
            let started = Instant::now();
            let addrs = tokio::net::lookup_host((host, 0)).await;
            let took = started.elapsed();
            crate::http::trace(|t| *t.dns.get_or_insert_default() += took);
            Ok(Box::new(addrs?) as reqwest::dns::Addrs)
        })
    }
}

fn dns_resolve(host: String) -> Option<String> {
    use std::net::ToSocketAddrs;
    (host.as_str(), 0)
        .to_socket_addrs()
        .ok()?
        .find(|a| a.is_ipv4())
        .map(|a| a.ip().to_string())
}

fn my_ip_address() -> String {
    // Connecting a UDP socket sends nothing; it just makes the OS pick the outbound interface.
    std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("8.8.8.8:53").and_then(|()| s.local_addr()))
        .map_or_else(|_| "127.0.0.1".into(), |a| a.ip().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Counts the connections it opens.
    struct Dial(Arc<std::sync::atomic::AtomicUsize>);

    impl tower_service::Service<()> for Dial {
        type Response = ();
        type Error = String;
        type Future =
            std::pin::Pin<Box<dyn std::future::Future<Output = Result<(), String>> + Send>>;

        fn poll_ready(
            &mut self,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<Result<(), String>> {
            std::task::Poll::Ready(Ok(()))
        }

        fn call(&mut self, _: ()) -> Self::Future {
            let dialed = self.0.clone();
            Box::pin(async move {
                dialed.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Ok(())
            })
        }
    }

    /// gRPC and WebSocket clients are built from the request's settings, with the HTTP
    /// version each must speak: a gRPC call to a self-signed server can skip the check.
    #[test]
    fn grpc_and_websocket_clients_take_the_requests_settings() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let net = Network {
            proxy: ProxyMode::None,
            ..Default::default()
        };
        let clients = rt.block_on(build_client(net)).unwrap();
        let unchecked = Settings {
            verify_tls: false,
            http_version: HttpVersion::Http1,
            ..Default::default()
        };
        clients.grpc_for(&unchecked, "http://x").unwrap();
        clients.websocket_for(&unchecked, "http://x").unwrap();
        let built = clients.variants.built.lock().unwrap();
        let mut got: Vec<_> = (built.keys()).map(|v| (v.version, v.insecure)).collect();
        got.sort_by_key(|(v, _)| *v == HttpVersion::Http1);
        assert_eq!(
            got,
            [(HttpVersion::Http2, true), (HttpVersion::Http1, true)]
        );
    }

    /// An authenticate message on a new connection is refused, so while a handshake lasts
    /// its client opens no second one; only a first connection the server closed (no end
    /// of the handshake within `NTLM_WAIT`) lets one through. Deterministic, where the
    /// handshake test only fails when hyper's pool happens to lose the race.
    #[test]
    fn an_ntlm_handshake_dials_once_while_it_lasts() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering::SeqCst};
        use tower_service::Service as _;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dialed = Arc::new(AtomicUsize::new(0));
        let gate = |opened: bool, over| OneConnection {
            inner: Dial(dialed.clone()),
            handshake: Handshake {
                opened: Arc::new(AtomicBool::new(opened)),
                over,
            },
        };
        rt.block_on(async {
            let (over, waiting) = tokio::sync::watch::channel(());
            let mut handshake = gate(false, waiting);
            handshake.call(()).await.unwrap();
            let second = tokio::spawn(handshake.call(()));
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(dialed.load(SeqCst), 1, "the second connect waits");
            drop(over);
            assert!(second.await.unwrap().is_err());
            assert_eq!(
                dialed.load(SeqCst),
                1,
                "and never dials once the handshake ends"
            );

            let (_over, waiting) = tokio::sync::watch::channel(());
            gate(true, waiting).call(()).await.unwrap();
            assert_eq!(dialed.load(SeqCst), 2, "dials after NTLM_WAIT");
        });
    }

    #[test]
    fn pac_routes_like_a_corporate_script() {
        let pac = Pac::new(
            r#"function FindProxyForURL(url, host) {
                 if (isPlainHostName(host) || dnsDomainIs(host, ".corp.local")) return "DIRECT";
                 if (isInNet(host, "10.0.0.0", "255.0.0.0")) return "DIRECT";
                 if (shExpMatch(host, "*.github.com")) return "SOCKS s:1080; PROXY gh:3128";
                 if (shExpMatch(host, "*.lab")) return "SOCKS5 s5:1080";
                 if (shExpMatch(host, "*.odd")) return "QUIC q:1; PROXY p:3128";
                 return "PROXY proxy.corp.local:8080; DIRECT";
               }"#,
        )
        .unwrap();
        let at = |u: &str| pac.proxy_for(&reqwest::Url::parse(u).unwrap());
        assert_eq!(at("http://intranet/x"), None);
        assert_eq!(at("https://wiki.corp.local/"), None);
        assert_eq!(at("http://10.1.2.3:8080/"), None);
        assert_eq!(
            at("https://api.github.com/"),
            Some("socks4a://s:1080".into())
        );
        assert_eq!(at("http://box.lab/"), Some("socks5h://s5:1080".into()));
        // An entry it can't use falls through to the next, rather than going direct.
        assert_eq!(at("http://x.odd/"), Some("http://p:3128".into()));
        assert_eq!(
            at("https://example.com/"),
            Some("http://proxy.corp.local:8080".into())
        );
    }

    /// A manual socks5h:// proxy carries the request, and gets the host name to resolve
    /// itself (what an intranet behind one needs): played by a minimal RFC 1928 proxy
    /// that sends every CONNECT to the echo server.
    #[test]
    fn requests_go_through_a_socks5_proxy() {
        use std::io::{Read as _, Write as _};
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let target = crate::http::tests::serve(|_| ("200 OK".into(), "via socks".into()));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let proxy = listener.local_addr().unwrap();
        let asked = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
        let seen = asked.clone();
        std::thread::spawn(move || {
            let (mut c, _) = listener.accept().unwrap();
            let mut b = [0u8; 262];
            c.read_exact(&mut b[..2]).unwrap(); // version, method count
            let n = b[1] as usize;
            c.read_exact(&mut b[..n]).unwrap();
            c.write_all(&[5, 0]).unwrap(); // no authentication
            c.read_exact(&mut b[..4]).unwrap(); // version, CONNECT, reserved, address type
            assert_eq!(b[3], 3, "a domain name, for the proxy to resolve");
            c.read_exact(&mut b[..1]).unwrap();
            let len = b[0] as usize;
            c.read_exact(&mut b[..len + 2]).unwrap();
            *seen.lock().unwrap() = String::from_utf8_lossy(&b[..len]).into_owned();
            c.write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0]).unwrap();
            let mut up = std::net::TcpStream::connect(target).unwrap();
            let (mut c2, mut up2) = (c.try_clone().unwrap(), up.try_clone().unwrap());
            std::thread::spawn(move || std::io::copy(&mut c2, &mut up2));
            let _ = std::io::copy(&mut up, &mut c);
        });
        let net = Network {
            proxy: ProxyMode::Manual,
            proxy_url: format!("socks5h://{proxy}"),
            ..Default::default()
        };
        let client = rt.block_on(build_client(net)).unwrap().http;
        let body = rt.block_on(async {
            let r = client
                .get("http://api.intranet.test/x")
                .send()
                .await
                .unwrap();
            r.text().await.unwrap()
        });
        assert_eq!(body, "via socks");
        assert_eq!(*asked.lock().unwrap(), "api.intranet.test");
    }

    #[test]
    fn a_system_pac_that_is_a_web_page_means_direct_not_broken() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        // What WPAD found on a real network: wpad.<domain> served the intranet home page.
        let page = crate::http::tests::json_server("<!DOCTYPE html><html></html>".into());
        let (proxy, note) = rt.block_on(system_proxy(Some(page.clone())));
        assert!(proxy.is_none());
        let note = note.unwrap();
        assert!(note.contains(&page) && note.contains("directly"), "{note}");
        let script = r#"function FindProxyForURL(u, h) { return "PROXY p:8080"; }"#;
        let (proxy, note) = rt.block_on(system_proxy(Some(crate::http::tests::json_server(
            script.into(),
        ))));
        assert!(proxy.is_some() && note.is_none());
        assert!(rt.block_on(system_proxy(None)).0.is_none());
    }

    #[test]
    fn mqtt_gets_the_proxy_https_would_use() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let route = |net: Network| rt.block_on(build_client(net)).unwrap().route;
        let at = |route: &Route, url: &str| {
            let hit = route.proxy_for(&reqwest::Url::parse(url).unwrap());
            hit.map(|(proxy, login)| (proxy.to_string(), login))
        };
        let manual = route(Network {
            proxy: ProxyMode::Manual,
            proxy_url: "http://u:p%40ss@proxy.test:3128".into(),
            no_proxy: "intra.test".into(),
            ..Default::default()
        });
        let login = Some(("u".to_owned(), "p@ss".to_owned()));
        assert_eq!(
            at(&manual, "https://broker.test:8883"),
            Some(("http://proxy.test:3128/".into(), login))
        );
        assert_eq!(at(&manual, "https://mq.intra.test:8883"), None);

        let path = std::env::temp_dir().join(format!("apitool-{}.pac", std::process::id()));
        let script = r#"function FindProxyForURL(url, host) {
                          return host == "broker.test" ? "PROXY pac.test:8080" : "DIRECT";
                        }"#;
        std::fs::write(&path, script).unwrap();
        let pac = route(Network {
            proxy: ProxyMode::Pac,
            pac_url: path.display().to_string(),
            ..Default::default()
        });
        std::fs::remove_file(&path).unwrap();
        assert_eq!(
            at(&pac, "https://broker.test:8883"),
            Some(("http://pac.test:8080/".into(), None))
        );
        assert_eq!(at(&pac, "https://other.test:8883"), None);

        let direct = route(Network {
            proxy: ProxyMode::None,
            ..Default::default()
        });
        assert_eq!(at(&direct, "https://broker.test:8883"), None);
    }

    #[test]
    fn pac_errors_are_readable() {
        let err = Pac::new("function FindProxyForURL( {").err().unwrap();
        assert!(err.contains("PAC script"), "{err}");
        let err = Pac::new("var x = 1;").err().unwrap();
        assert!(err.contains("FindProxyForURL is not defined"), "{err}");
    }

    #[test]
    fn host_certificates_match_by_name_wildcard_and_port() {
        let at = |pattern: &str, url: &str| {
            let net = Network {
                client_cert: "default.pem".into(),
                host_certs: vec![HostCert {
                    host: pattern.into(),
                    cert: "host.pem".into(),
                    ..Default::default()
                }],
                ..Default::default()
            };
            net.cert_for(url).0.to_owned()
        };
        assert_eq!(at("API.corp.com", "https://api.corp.com/v1"), "host.pem");
        assert_eq!(at("api.corp.com", "https://corp.com/"), "default.pem");
        // A wildcard covers subdomains, not the domain or look-alikes.
        assert_eq!(at("*.corp.com", "https://a.b.corp.com/"), "host.pem");
        assert_eq!(at("*.corp.com", "https://corp.com/"), "default.pem");
        assert_eq!(at("*.corp.com", "https://evilcorp.com/"), "default.pem");
        // The port, when given, is compared with the URL's, defaults included.
        assert_eq!(at("api.corp.com:443", "https://api.corp.com/"), "host.pem");
        assert_eq!(
            at("api.corp.com:8443", "https://api.corp.com/"),
            "default.pem"
        );
        assert_eq!(at("[::1]:8443", "https://[::1]:8443/"), "host.pem");
        assert_eq!(at("[::1]", "https://[::1]:8443/"), "host.pem");
        assert_eq!(at("", "https://api.corp.com/"), "default.pem");
    }

    #[test]
    fn pfx_client_certificate_loads() {
        let cert = rcgen::generate_simple_self_signed(vec!["client.test".into()]).unwrap();
        let key = p12_keystore::PrivateKey::from_der(&cert.signing_key.serialize_der()).unwrap();
        let x509 = p12_keystore::Certificate::from_der(cert.cert.der()).unwrap();
        let mut store = p12_keystore::KeyStore::new();
        let chain = p12_keystore::PrivateKeyChain::new(vec![1u8; 20], key, [x509]);
        store.add_entry(
            "client",
            p12_keystore::KeyStoreEntry::PrivateKeyChain(chain),
        );
        let pfx = store.writer("pw").write().unwrap();

        let path = std::env::temp_dir().join(format!("apitool-{}.pfx", std::process::id()));
        std::fs::write(&path, &pfx).unwrap();
        assert!(load_identity(&path, "pw").is_ok());
        assert!(
            load_identity(&path, "wrong")
                .unwrap_err()
                .contains("wrong password")
        );
        std::fs::remove_file(&path).unwrap();
    }
}
