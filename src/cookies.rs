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

/// The cookies a response sets, in header order. Domain and path are as the header gives
/// them (empty: the request's host and path).
pub fn from_response(headers: &[(String, String)]) -> Vec<Row> {
    let set = headers
        .iter()
        .filter(|(k, _)| k.eq_ignore_ascii_case("set-cookie"));
    set.filter_map(|(_, v)| RawCookie::parse(v.as_str()).ok())
        .map(|c| Row {
            domain: c.domain().unwrap_or_default().to_owned(),
            path: c.path().unwrap_or_default().to_owned(),
            name: c.name().to_owned(),
            value: c.value().to_owned(),
            expires: match (c.max_age(), c.expires_datetime()) {
                (Some(age), _) => Some(format!("in {} s", age.whole_seconds())),
                (None, Some(t)) => Some(format!(
                    "{} {:02}:{:02} UTC",
                    t.date(),
                    t.hour(),
                    t.minute()
                )),
                (None, None) => None,
            },
        })
        .collect()
}

impl Jar {
    fn read(&self) -> RwLockReadGuard<'_, CookieStore> {
        self.0.read().unwrap_or_else(|e| e.into_inner())
    }

    fn write(&self) -> RwLockWriteGuard<'_, CookieStore> {
        self.0.write().unwrap_or_else(|e| e.into_inner())
    }

    /// Missing or unreadable JSON is an empty jar: cookies are a cache the server refills.
    /// Expired cookies are left out, so the next save stops carrying them.
    pub fn from_json(json: &str) -> Self {
        let store = cookie_store::serde::json::load(json.as_bytes()).unwrap_or_default();
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

    /// What a request to `url` would send, as (name, value).
    pub fn for_url(&self, url: &Url) -> Vec<(String, String)> {
        (self.read().get_request_values(url))
            .map(|(n, v)| (n.to_owned(), v.to_owned()))
            .collect()
    }

    /// As if `url` had answered `Set-Cookie: name=value` (a session cookie for its host).
    pub fn set(&self, url: &Url, name: &str, value: &str) -> Result<(), String> {
        let cookie = RawCookie::parse(format!("{name}={value}"))
            .map_err(|e| format!("cookie {name}: {e}"))?;
        (self.write().insert_raw(&cookie, url))
            .map(drop)
            .map_err(|e| format!("cookie {name}: {e}"))
    }

    /// Removes the cookies named `name` (all of them: None) that `url` would be sent.
    pub fn unset(&self, url: &Url, name: Option<&str>) {
        let mut store = self.write();
        let doomed: Vec<(String, String, String)> = (store.matches(url).into_iter())
            .filter(|c| name.is_none_or(|n| c.name() == n))
            .map(|c| {
                (
                    String::from(&c.domain),
                    String::from(&c.path),
                    c.name().to_owned(),
                )
            })
            .collect();
        for (domain, path, name) in doomed {
            store.remove(&domain, &path, &name);
        }
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
        if pairs.is_empty() {
            return None;
        }
        let value = pairs.join("; ");
        // The first hop's, for the Timeline.
        crate::http::trace(|t| {
            t.cookie.get_or_insert_with(|| value.clone());
        });
        HeaderValue::from_str(&value).ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expired_cookies_are_not_carried_forever() {
        let url = Url::parse("https://example.com/").unwrap();
        let jar = Jar::default();
        let headers = [
            // 1 Jan is a Monday in both years, so the date stays valid after the move below.
            HeaderValue::from_static("old=1; Expires=Mon, 01 Jan 2035 00:00:00 GMT"),
            HeaderValue::from_static("session=2"),
        ];
        reqwest::cookie::CookieStore::set_cookies(&jar, &mut headers.iter(), &url);
        // Time passes: the saved expiry moves into the past.
        let saved = jar.to_json().unwrap().replace("2035", "2001");
        assert!(saved.contains("old=1"));
        let next = Jar::from_json(&saved).to_json().unwrap();
        assert!(!next.contains("old=1"), "{next}");
        assert!(
            next.contains("session=2"),
            "a session cookie survives a restart"
        );
    }
}
