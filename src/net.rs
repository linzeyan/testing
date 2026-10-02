//! Per-machine network settings (proxy, PAC, CA, client certificate) and the HTTP client
//! built from them. Stored in the gitignored `.state.toml`: proxies and cert paths differ
//! per machine, and the PFX password must never reach git.

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use base64::Engine as _;
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
    pub insecure: bool,
    pub timeout_secs: u64,
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
            insecure: false,
            timeout_secs: 60,
        }
    }
}

/// gRPC needs HTTP/2 even over plain TCP (h2c), which reqwest only does with prior
/// knowledge, and that would break HTTP/1-only servers for everything else.
#[derive(Clone)]
pub struct Clients {
    pub http: reqwest::Client,
    pub grpc: reqwest::Client,
    /// Why the system's PAC script was skipped, to show next to the proxy mode.
    pub note: Option<String>,
    variants: Arc<Variants>,
}

/// What request settings change about the connection itself (the timeout is set per
/// request instead). reqwest fixes these per client, so each combination in use gets
/// its own, built once.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Variant {
    version: HttpVersion,
    /// None doesn't follow.
    redirects: Option<u32>,
    insecure: bool,
    cookies: bool,
}

impl From<&Settings> for Variant {
    fn from(s: &Settings) -> Self {
        Self {
            version: s.http_version,
            redirects: s.follow_redirects.then_some(s.max_redirects),
            insecure: !s.verify_tls,
            cookies: s.cookies,
        }
    }
}

struct Variants {
    build: Box<dyn Fn(Variant) -> Result<reqwest::Client, String> + Send + Sync>,
    built: Mutex<HashMap<Variant, reqwest::Client>>,
}

impl Clients {
    /// The client for a request's settings; the shared one unless they differ from the
    /// defaults.
    pub fn for_settings(&self, s: &Settings) -> Result<reqwest::Client, String> {
        let v = Variant::from(s);
        if v == Variant::from(&Settings::default()) {
            return Ok(self.http.clone());
        }
        if let Some(c) = self.variants.built.lock().unwrap().get(&v) {
            return Ok(c.clone());
        }
        let c = (self.variants.build)(v)?;
        self.variants.built.lock().unwrap().insert(v, c.clone());
        Ok(c)
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
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Any explicit `.proxy()` turns off reqwest's own system-proxy lookup.
    let mut note = None;
    let proxy = match net.proxy {
        ProxyMode::System => {
            let (proxy, skipped) = system_proxy(system_pac_url().await?).await;
            note = skipped;
            proxy
        }
        ProxyMode::None => None,
        ProxyMode::Manual => {
            let proxy = reqwest::Proxy::all(net.proxy_url.trim())
                .map_err(|e| format!("proxy URL: {}", error_chain(&e)))?;
            Some(proxy.no_proxy(reqwest::NoProxy::from_string(&net.no_proxy)))
        }
        ProxyMode::Pac => Some(pac_proxy(net.pac_url.trim()).await?),
    };

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

    let cert = net.client_cert.trim();
    let identity = if cert.is_empty() {
        None
    } else {
        Some(load_identity(Path::new(cert), &net.client_cert_password)?)
    };

    let build = move |v: Variant| {
        let mut b = reqwest::Client::builder()
            .timeout(Duration::from_secs(net.timeout_secs.max(1)))
            .tls_danger_accept_invalid_certs(net.insecure || v.insecure)
            .redirect(match v.redirects {
                Some(n) => reqwest::redirect::Policy::limited(n as usize),
                None => reqwest::redirect::Policy::none(),
            });
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
        if let Some(id) = &identity {
            b = b.identity(id.clone());
        }
        b = match v.version {
            HttpVersion::Auto => b,
            HttpVersion::Http1 => b.http1_only(),
            // Over TLS this offers only h2; over plain TCP it is h2c.
            HttpVersion::Http2 => b.http2_prior_knowledge(),
        };
        b.build().map_err(|e| error_chain(&e))
    };
    let default = Variant::from(&Settings::default());
    let grpc = Variant {
        version: HttpVersion::Http2,
        ..default
    };
    Ok(Clients {
        http: build(default)?,
        grpc: build(grpc)?,
        note,
        variants: Arc::new(Variants {
            build: Box::new(build),
            built: Default::default(),
        }),
    })
}

/// The system's PAC script, or direct when it can't be used, as browsers do: WPAD often
/// finds an intranet web page instead of a script. The second value says why it was skipped.
async fn system_proxy(pac: Option<String>) -> (Option<reqwest::Proxy>, Option<String>) {
    let Some(url) = pac else {
        return (None, None);
    };
    match pac_proxy(&url).await {
        Ok(proxy) => (Some(proxy), None),
        Err(e) => (
            None,
            Some(format!(
                "The system's proxy script {url} can't be used, so requests go out directly \
                 (as in a browser).\n{e}"
            )),
        ),
    }
}

/// Accepts a PEM file (key + certificate chain) or a PFX/P12 bundle.
fn load_identity(path: &Path, password: &str) -> Result<reqwest::Identity, String> {
    let shown = path.display();
    let data = std::fs::read(path).map_err(|e| format!("client certificate {shown}: {e}"))?;
    let is_pkcs12 = path
        .extension()
        .and_then(|x| x.to_str())
        .is_some_and(|x| x.eq_ignore_ascii_case("pfx") || x.eq_ignore_ascii_case("p12"));
    let pem = if is_pkcs12 {
        pkcs12_to_pem(&data, password)?
    } else {
        data
    };
    reqwest::Identity::from_pem(&pem)
        .map_err(|e| format!("client certificate {shown}: {}", error_chain(&e)))
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
unsafe fn take_wide(p: windows_sys::core::PWSTR) -> Option<String> {
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
    match system_auto_config() {
        (Some(url), _) => Ok(Some(url)),
        (None, true) => tokio::task::spawn_blocking(detect_wpad)
            .await
            .map_err(|e| format!("WPAD: {e}")),
        (None, false) => Ok(None),
    }
}

async fn pac_proxy(location: &str) -> Result<reqwest::Proxy, String> {
    let script = fetch_pac(location).await?;
    let pac = Pac::new(&script)?;
    Ok(reqwest::Proxy::custom(move |url| pac.proxy_for(url)))
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
        return resp.text().await.map_err(|e| err(&e));
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
        let proxy = self
            .find(url.as_str(), host)
            .ok()
            .and_then(|r| first_proxy(&r));
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
            _ => {} // SOCKS isn't compiled in; fall through to the next entry
        }
    }
    None
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

    #[test]
    fn pac_routes_like_a_corporate_script() {
        let pac = Pac::new(
            r#"function FindProxyForURL(url, host) {
                 if (isPlainHostName(host) || dnsDomainIs(host, ".corp.local")) return "DIRECT";
                 if (isInNet(host, "10.0.0.0", "255.0.0.0")) return "DIRECT";
                 if (shExpMatch(host, "*.github.com")) return "SOCKS s:1080; PROXY gh:3128";
                 return "PROXY proxy.corp.local:8080; DIRECT";
               }"#,
        )
        .unwrap();
        let at = |u: &str| pac.proxy_for(&reqwest::Url::parse(u).unwrap());
        assert_eq!(at("http://intranet/x"), None);
        assert_eq!(at("https://wiki.corp.local/"), None);
        assert_eq!(at("http://10.1.2.3:8080/"), None);
        // SOCKS is unsupported, so the next entry must be used instead of going direct.
        assert_eq!(at("https://api.github.com/"), Some("http://gh:3128".into()));
        assert_eq!(
            at("https://example.com/"),
            Some("http://proxy.corp.local:8080".into())
        );
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
    fn pac_errors_are_readable() {
        let err = Pac::new("function FindProxyForURL( {").err().unwrap();
        assert!(err.contains("PAC script"), "{err}");
        let err = Pac::new("var x = 1;").err().unwrap();
        assert!(err.contains("FindProxyForURL is not defined"), "{err}");
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
