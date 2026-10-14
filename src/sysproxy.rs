//! Points the system's proxy at the capture and back, as Proxyman and Fiddler do. What was
//! set before is read first (`current`), so the caller can keep it somewhere a crash
//! doesn't lose, then put back by `restore`: on Stop, on quit, or at the next start.

use std::net::SocketAddr;

pub use imp::Saved;

/// The system's proxy settings as they are now, to `restore` later.
pub fn current() -> Result<Saved, String> {
    imp::current()
}

/// The system's HTTP and HTTPS proxy at `addr`, a script or auto-detection off (either
/// would win over it). What fails half way puts `saved` back.
pub fn point(saved: &Saved, addr: SocketAddr) -> Result<(), String> {
    imp::point(saved, addr).inspect_err(|_| {
        let _ = imp::restore(saved);
    })
}

pub fn restore(saved: &Saved) -> Result<(), String> {
    imp::restore(saved)
}

/// macOS by `networksetup`, per network service. The commands are worked out here, apart
/// from running them, so a test can read them without touching the machine's settings.
#[cfg(any(target_os = "macos", test))]
mod mac {
    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
    pub struct Manual {
        pub on: bool,
        pub host: String,
        pub port: u16,
    }

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Service {
        pub name: String,
        pub web: Manual,
        pub secure: Manual,
        /// Its proxy script was on.
        pub pac: bool,
    }

    /// `-listallnetworkservices`: a note first, then a service a line, `*` before one
    /// that's off.
    pub fn services(listed: &str) -> Vec<String> {
        (listed.lines().skip(1))
            .filter(|l| !l.trim().is_empty() && !l.starts_with('*'))
            .map(str::to_owned)
            .collect()
    }

    /// `-getwebproxy`, `-getsecurewebproxy` ("Enabled: Yes", "Server: …", "Port: …") and
    /// `-getautoproxyurl` (its "Enabled").
    pub fn manual(shown: &str) -> Manual {
        let field = |name: &str| {
            (shown.lines())
                .find_map(|l| l.strip_prefix(name)?.strip_prefix(':'))
                .unwrap_or_default()
                .trim()
        };
        Manual {
            on: field("Enabled") == "Yes",
            host: field("Server").to_owned(),
            port: field("Port").parse().unwrap_or(0),
        }
    }

    pub fn point(s: &Service, host: &str, port: u16) -> Vec<Vec<String>> {
        let port = port.to_string();
        let mut run = vec![
            args(["-setwebproxy", &s.name, host, &port]),
            args(["-setsecurewebproxy", &s.name, host, &port]),
        ];
        if s.pac {
            run.push(args(["-setautoproxystate", &s.name, "off"]));
        }
        run
    }

    pub fn back(s: &Service) -> Vec<Vec<String>> {
        let mut run = Vec::new();
        let kinds = [
            ("-setwebproxy", "-setwebproxystate", &s.web),
            ("-setsecurewebproxy", "-setsecurewebproxystate", &s.secure),
        ];
        for (set, state, was) in kinds {
            // Setting one turns it on: one that was off gets its server back, then goes off.
            if !was.host.is_empty() {
                run.push(args([set, &s.name, &was.host, &was.port.to_string()]));
            }
            if !was.on {
                run.push(args([state, &s.name, "off"]));
            }
        }
        if s.pac {
            run.push(args(["-setautoproxystate", &s.name, "on"]));
        }
        run
    }

    fn args<const N: usize>(a: [&str; N]) -> Vec<String> {
        a.map(str::to_owned).to_vec()
    }
}

#[cfg(target_os = "macos")]
mod imp {
    use std::net::SocketAddr;
    use std::process::Command;

    use super::mac::{self, Service};

    pub type Saved = Vec<Service>;

    fn run(args: &[String]) -> Result<String, String> {
        let shown = || format!("networksetup {}", args.join(" "));
        let out = (Command::new("/usr/sbin/networksetup").args(args).output())
            .map_err(|e| format!("{}: {e}", shown()))?;
        let text = String::from_utf8_lossy(&out.stdout).into_owned();
        // Some failures are told on stdout, with success.
        if !out.status.success() || text.starts_with("** Error") {
            let err = String::from_utf8_lossy(&out.stderr);
            return Err(format!("{}: {} {}", shown(), text.trim(), err.trim()));
        }
        Ok(text)
    }

    fn get(what: &str, service: &str) -> Result<mac::Manual, String> {
        run(&[what.to_owned(), service.to_owned()]).map(|shown| mac::manual(&shown))
    }

    pub fn current() -> Result<Saved, String> {
        let listed = run(&["-listallnetworkservices".to_owned()])?;
        (mac::services(&listed).into_iter())
            .map(|name| {
                Ok(Service {
                    web: get("-getwebproxy", &name)?,
                    secure: get("-getsecurewebproxy", &name)?,
                    pac: get("-getautoproxyurl", &name)?.on,
                    name,
                })
            })
            .collect()
    }

    pub fn point(saved: &Saved, addr: SocketAddr) -> Result<(), String> {
        let host = addr.ip().to_string();
        for s in saved {
            for args in mac::point(s, &host, addr.port()) {
                run(&args)?;
            }
        }
        Ok(())
    }

    /// Every service, even past one that fails: the first failure is told.
    pub fn restore(saved: &Saved) -> Result<(), String> {
        let mut failed = Ok(());
        for args in saved.iter().flat_map(mac::back) {
            if let Err(e) = run(&args) {
                failed = failed.and(Err(e));
            }
        }
        failed
    }
}

/// Windows by WinINet's per-connection options, as the Internet Options dialog sets them:
/// written to the registry and told to running apps in one go.
#[cfg(windows)]
mod imp {
    use std::ffi::c_void;
    use std::net::SocketAddr;
    use std::ptr::{null, null_mut};

    use serde::{Deserialize, Serialize};
    use windows_sys::Win32::Networking::WinInet::*;

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Saved {
        /// `PROXY_TYPE_*`: direct, a proxy, a script, auto-detection.
        flags: u32,
        server: Option<String>,
        bypass: Option<String>,
        pac: Option<String>,
    }

    type List = INTERNET_PER_CONN_OPTION_LISTW;

    fn list(options: &mut [INTERNET_PER_CONN_OPTIONW]) -> List {
        List {
            dwSize: size_of::<List>() as u32,
            // The LAN settings, which a VDI's network uses.
            pszConnection: null_mut(),
            dwOptionCount: options.len() as u32,
            dwOptionError: 0,
            pOptions: options.as_mut_ptr(),
        }
    }

    fn option(id: u32) -> INTERNET_PER_CONN_OPTIONW {
        INTERNET_PER_CONN_OPTIONW {
            dwOption: id,
            ..Default::default()
        }
    }

    fn last_error(what: &str) -> String {
        format!("{what}: {}", std::io::Error::last_os_error())
    }

    pub fn current() -> Result<Saved, String> {
        // FLAGS_UI, as the dialog shows them: FLAGS loses auto-detection once it found
        // nothing, and putting that back would leave it off.
        let ids = [
            INTERNET_PER_CONN_FLAGS_UI,
            INTERNET_PER_CONN_PROXY_SERVER,
            INTERNET_PER_CONN_PROXY_BYPASS,
            INTERNET_PER_CONN_AUTOCONFIG_URL,
        ];
        let mut options = ids.map(option);
        let mut l = list(&mut options);
        let mut len = size_of::<List>() as u32;
        let p = (&mut l as *mut List).cast::<c_void>();
        // SAFETY: `l` points at `options`, alive for the call; the strings it fills in are
        // GlobalAlloc'd and ours, taken (and freed) by `take_wide` below.
        if unsafe {
            InternetQueryOptionW(null(), INTERNET_OPTION_PER_CONNECTION_OPTION, p, &mut len)
        } == 0
        {
            return Err(last_error("reading the system proxy"));
        }
        unsafe {
            Ok(Saved {
                flags: options[0].Value.dwValue,
                server: crate::net::take_wide(options[1].Value.pszValue),
                bypass: crate::net::take_wide(options[2].Value.pszValue),
                pac: crate::net::take_wide(options[3].Value.pszValue),
            })
        }
    }

    fn set(
        flags: u32,
        server: Option<&str>,
        bypass: Option<&str>,
        pac: Option<&str>,
    ) -> Result<(), String> {
        let wide = |s: Option<&str>| s.map(|s| s.encode_utf16().chain([0]).collect::<Vec<u16>>());
        let mut texts = [wide(server), wide(bypass), wide(pac)];
        let ptr = |w: &mut Option<Vec<u16>>| w.as_mut().map_or(null_mut(), |w| w.as_mut_ptr());
        let mut options = [
            INTERNET_PER_CONN_FLAGS,
            INTERNET_PER_CONN_PROXY_SERVER,
            INTERNET_PER_CONN_PROXY_BYPASS,
            INTERNET_PER_CONN_AUTOCONFIG_URL,
        ]
        .map(option);
        options[0].Value.dwValue = flags;
        for (o, text) in options[1..].iter_mut().zip(&mut texts) {
            o.Value.pszValue = ptr(text);
        }
        let l = list(&mut options);
        let p = (&l as *const List).cast::<c_void>();
        // SAFETY: `l` points at `options` and they at `texts`, all alive for the calls,
        // which only read them.
        unsafe {
            if InternetSetOptionW(
                null(),
                INTERNET_OPTION_PER_CONNECTION_OPTION,
                p,
                size_of::<List>() as u32,
            ) == 0
            {
                return Err(last_error("setting the system proxy"));
            }
            // What running apps listen for to read their settings again.
            InternetSetOptionW(null(), INTERNET_OPTION_SETTINGS_CHANGED, null(), 0);
            InternetSetOptionW(null(), INTERNET_OPTION_REFRESH, null(), 0);
        }
        Ok(())
    }

    pub fn point(_: &Saved, addr: SocketAddr) -> Result<(), String> {
        let flags = PROXY_TYPE_DIRECT | PROXY_TYPE_PROXY;
        // Everything else through the capture, intranet hosts too: they are what's tried.
        set(
            flags,
            Some(&addr.to_string()),
            Some("localhost;127.0.0.1"),
            None,
        )
    }

    pub fn restore(s: &Saved) -> Result<(), String> {
        set(
            s.flags,
            s.server.as_deref(),
            s.bypass.as_deref(),
            s.pac.as_deref(),
        )
    }
}

/// ponytail: Linux desktops each keep their own settings (GNOME's gsettings, KDE's files);
/// add GNOME's when someone captures on Linux.
#[cfg(not(any(target_os = "macos", windows)))]
mod imp {
    use std::net::SocketAddr;

    use serde::{Deserialize, Serialize};

    #[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
    pub struct Saved;

    pub fn current() -> Result<Saved, String> {
        Err("apitool can't set this system's proxy: set the address shown by hand".into())
    }

    pub fn point(_: &Saved, _: SocketAddr) -> Result<(), String> {
        current().map(|_| ())
    }

    pub fn restore(_: &Saved) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::mac::*;

    #[test]
    fn the_services_and_their_proxies_read_as_networksetup_shows_them() {
        let listed = "An asterisk (*) denotes that a network service is disabled.\n\
                      Wi-Fi\n*Bluetooth PAN\nUSB 10/100/1000 LAN\n";
        assert_eq!(services(listed), ["Wi-Fi", "USB 10/100/1000 LAN"]);
        let shown =
            "Enabled: Yes\nServer: proxy.corp\nPort: 8080\nAuthenticated Proxy Enabled: 0\n";
        let corp = Manual {
            on: true,
            host: "proxy.corp".into(),
            port: 8080,
        };
        assert_eq!(manual(shown), corp);
        assert_eq!(
            manual("Enabled: No\nServer: \nPort: 0\n"),
            Manual::default()
        );
        assert!(manual("URL: http://wpad.corp/proxy.pac\nEnabled: Yes\n").on);
    }

    /// Stop puts back what was there: the corporate proxy on with its server, the one
    /// that was off off again, the proxy script back on (it was turned off: it would have
    /// won over the capture).
    #[test]
    fn a_service_is_put_back_as_it_was() {
        let s = Service {
            name: "Wi-Fi".into(),
            web: Manual {
                on: true,
                host: "proxy.corp".into(),
                port: 8080,
            },
            secure: Manual {
                on: false,
                host: "old.corp".into(),
                port: 3128,
            },
            pac: true,
        };
        let line = |run: Vec<Vec<String>>| run.into_iter().map(|a| a.join(" ")).collect::<Vec<_>>();
        assert_eq!(
            line(point(&s, "127.0.0.1", 9090)),
            [
                "-setwebproxy Wi-Fi 127.0.0.1 9090",
                "-setsecurewebproxy Wi-Fi 127.0.0.1 9090",
                "-setautoproxystate Wi-Fi off",
            ]
        );
        assert_eq!(
            line(back(&s)),
            [
                "-setwebproxy Wi-Fi proxy.corp 8080",
                "-setsecurewebproxy Wi-Fi old.corp 3128",
                "-setsecurewebproxystate Wi-Fi off",
                "-setautoproxystate Wi-Fi on",
            ]
        );
        let bare = Service {
            web: Manual::default(),
            secure: Manual::default(),
            pac: false,
            ..s
        };
        assert_eq!(
            line(back(&bare)),
            [
                "-setwebproxystate Wi-Fi off",
                "-setsecurewebproxystate Wi-Fi off",
            ]
        );
    }
}
