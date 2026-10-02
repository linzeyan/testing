//! Cookie jar shared by every request, like a browser's: Set-Cookie responses are stored
//! (redirect hops included) and sent back to matching URLs. A request with its own Cookie
//! header sends only that.

use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use cookie_store::{CookieStore, RawCookie};
use reqwest::Url;
use reqwest::header::HeaderValue;

#[derive(Default)]
pub struct Jar(RwLock<CookieStore>);

/// One cookie as the manager lists it.
pub struct Row {
    pub domain: String,
    pub path: String,
    pub name: String,
    pub value: String,
    /// `None` for a session cookie.
    pub expires: Option<String>,
}

impl Jar {
    fn read(&self) -> RwLockReadGuard<'_, CookieStore> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, CookieStore> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Missing or unreadable JSON is an empty jar: cookies are a cache the server refills.
    pub fn from_json(json: &str) -> Self {
        let store = cookie_store::serde::json::load_all(json.as_bytes()).unwrap_or_default();
        Self(RwLock::new(store))
    }

    /// Session cookies are kept too: in an API client, "logged in" should survive a restart.
    pub fn to_json(&self) -> Result<String, String> {
        let mut out = Vec::new();
        cookie_store::serde::json::save_incl_expired_and_nonpersistent(&self.read(), &mut out)
            .map_err(|e| format!("cookies: {e}"))?;
        String::from_utf8(out).map_err(|e| format!("cookies: {e}"))
    }

    /// Unexpired cookies, by domain then name.
    pub fn rows(&self) -> Vec<Row> {
        let store = self.read();
        let mut rows: Vec<Row> = store
            .iter_unexpired()
            .map(|c| Row {
                domain: String::from(&c.domain),
                path: String::from(&c.path),
                name: c.name().to_owned(),
                value: c.value().to_owned(),
                expires: match &c.expires {
                    cookie_store::CookieExpiration::AtUtc(t) => Some(format!(
                        "{} {:02}:{:02} UTC",
                        t.date(),
                        t.hour(),
                        t.minute()
                    )),
                    cookie_store::CookieExpiration::SessionEnd => None,
                },
            })
            .collect();
        rows.sort_by(|a, b| (&a.domain, &a.name).cmp(&(&b.domain, &b.name)));
        rows
    }

    pub fn remove(&self, row: &Row) {
        self.write().remove(&row.domain, &row.path, &row.name);
    }

    pub fn clear(&self) {
        self.write().clear();
    }
}

impl reqwest::cookie::CookieStore for Jar {
    fn set_cookies(&self, headers: &mut dyn Iterator<Item = &HeaderValue>, url: &Url) {
        let cookies = headers
            .filter_map(|v| v.to_str().ok())
            .filter_map(|s| RawCookie::parse(s.to_owned()).ok());
        self.write().store_response_cookies(cookies, url);
    }

    fn cookies(&self, url: &Url) -> Option<HeaderValue> {
        let pairs: Vec<String> = self
            .read()
            .get_request_values(url)
            .map(|(name, value)| format!("{name}={value}"))
            .collect();
        (!pairs.is_empty())
            .then(|| HeaderValue::from_str(&pairs.join("; ")).ok())
            .flatten()
    }
}
