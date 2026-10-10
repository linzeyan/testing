//! Updates from apitool's GitHub releases: finding a newer one, and installing it in place.
//! The release archive is unpacked next to the running binaries, which are renamed aside
//! (Windows lets a running exe be renamed, not replaced) and deleted on the next start.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const LATEST: &str = "https://api.github.com/repos/linzeyan/testing/releases/latest";
pub const CURRENT: &str = env!("CARGO_PKG_VERSION");
/// Release archives are named `apitool-v<version>-<target>.<zip|tar.gz>`.
const TARGET: &str = env!("APITOOL_TARGET");
const BINARIES: [&str; 2] = ["apitool", "apitool-cli"];
/// Set by Restart now; main starts the new binary once the window has closed.
pub static RESTART: AtomicBool = AtomicBool::new(false);

/// Chosen in Settings.
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(default)]
pub struct Updates {
    pub check: Check,
    /// Install what a check finds without asking; it's used from the next start.
    pub install: bool,
    /// Unix seconds of the last check, for `Check::Daily` and `Weekly`.
    pub checked: u64,
}

impl Default for Updates {
    fn default() -> Self {
        Self {
            check: Check::Daily,
            install: false,
            checked: 0,
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub enum Check {
    Never,
    /// Every time apitool starts.
    AtStart,
    Daily,
    Weekly,
}

impl Updates {
    /// Whether an automatic check is due `now` (Unix seconds); `started` is whether this
    /// run has checked yet.
    pub fn due(&self, now: u64, started: bool) -> bool {
        let every = |days: u64| now.saturating_sub(self.checked) >= days * 24 * 60 * 60;
        match self.check {
            Check::Never => false,
            Check::AtStart => !started,
            Check::Daily => every(1),
            Check::Weekly => every(7),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Release {
    pub version: String,
    /// The release page, for its notes.
    pub page: String,
    /// The archive for this platform; None when the release has none.
    pub archive: Option<String>,
}

/// The latest release, from GitHub's JSON for it.
fn parse(json: &str) -> Result<Release, String> {
    #[derive(Deserialize)]
    struct Asset {
        name: String,
        browser_download_url: String,
    }
    #[derive(Deserialize)]
    struct Latest {
        tag_name: String,
        html_url: String,
        assets: Vec<Asset>,
    }
    let latest: Latest = serde_json::from_str(json).map_err(|e| format!("release: {e}"))?;
    let archive = (latest.assets.into_iter())
        .find(|a| a.name.contains(&format!("-{TARGET}.")))
        .map(|a| a.browser_download_url);
    Ok(Release {
        version: latest.tag_name.trim_start_matches('v').to_owned(),
        page: latest.html_url,
        archive,
    })
}

/// Whether `version` comes after this build's, number by number ("0.10.0" > "0.9.9").
pub fn is_newer(version: &str) -> bool {
    let numbers = |v: &str| -> Vec<u64> { v.split('.').map(|n| n.parse().unwrap_or(0)).collect() };
    numbers(version) > numbers(CURRENT)
}

pub async fn latest(client: &reqwest::Client) -> Result<Release, String> {
    let response = (client.get(LATEST))
        .header("Accept", "application/vnd.github+json")
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    let status = response.status();
    let text = response.text().await.map_err(|e| e.to_string())?;
    if !status.is_success() {
        return Err(format!("GitHub: HTTP {status}"));
    }
    parse(&text)
}

/// Downloads `release`'s archive and puts its binaries in place of the running ones.
pub async fn install(client: &reqwest::Client, release: &Release) -> Result<(), String> {
    install_into(client, release, here()?).await
}

async fn install_into(
    client: &reqwest::Client,
    release: &Release,
    dir: PathBuf,
) -> Result<(), String> {
    let url = (release.archive.as_deref())
        .ok_or_else(|| format!("{} has no build for {TARGET}", release.version))?;
    // Beside the binaries, so they move into place by rename on the same volume.
    let work = dir.join(".apitool-update");
    let _ = std::fs::remove_dir_all(&work);
    let create = |e: std::io::Error| format!("{}: {e}", work.display());
    std::fs::create_dir_all(&work).map_err(create)?;
    let archive = work.join(url.rsplit('/').next().unwrap_or("archive"));
    let downloaded = download(client, url, &archive).await;
    let result = downloaded
        .and_then(|()| unpack(&archive, &work))
        .and_then(|()| swap(&work, &dir));
    let _ = std::fs::remove_dir_all(&work);
    result
}

async fn download(client: &reqwest::Client, url: &str, to: &Path) -> Result<(), String> {
    use std::io::Write;
    let mut response = (client.get(url))
        .timeout(Duration::from_secs(600))
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("download: {e}"))?;
    let write = |e: std::io::Error| format!("{}: {e}", to.display());
    // In pieces: RAM is tight, and the archive is tens of MB.
    let mut file = std::fs::File::create(to).map_err(write)?;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|e| format!("download: {e}"))?
    {
        file.write_all(&chunk).map_err(write)?;
    }
    Ok(())
}

/// The system's tar reads both archives: bsdtar has shipped with Windows since 10 (1803)
/// and reads zip too, which saves a zip crate.
fn unpack(archive: &Path, into: &Path) -> Result<(), String> {
    let mut tar = std::process::Command::new("tar");
    tar.arg("-xf").arg(archive).arg("-C").arg(into);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console flashing up over the app.
        tar.creation_flags(0x0800_0000);
    }
    let out = tar.output().map_err(|e| format!("tar: {e}"))?;
    match out.status.success() {
        true => Ok(()),
        false => Err(format!(
            "tar: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        )),
    }
}

fn name(binary: &str) -> String {
    format!("{binary}{}", std::env::consts::EXE_SUFFIX)
}

/// Moves the binaries unpacked in `from` into `to`, renaming each one they replace to
/// `<name>.old`.
fn swap(from: &Path, to: &Path) -> Result<(), String> {
    let new = |b: &str| from.join(name(b));
    if !BINARIES.iter().all(|b| new(b).is_file()) {
        return Err("the archive doesn't hold both apitool binaries".into());
    }
    for binary in BINARIES {
        let (target, old) = (
            to.join(name(binary)),
            to.join(format!("{}.old", name(binary))),
        );
        let _ = std::fs::remove_file(&old);
        let fail = |e: std::io::Error| format!("{}: {e}", target.display());
        if target.exists() {
            std::fs::rename(&target, &old).map_err(fail)?;
        }
        if let Err(e) = std::fs::rename(new(binary), &target) {
            // Leave a working binary behind, if not the new one.
            let _ = std::fs::rename(&old, &target);
            return Err(fail(e));
        }
    }
    Ok(())
}

/// Where the running binary is.
fn here() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    exe.parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "the binary has no folder".into())
}

/// Deletes the binaries the last update renamed aside: they were running then. Retried for
/// a while on its own thread: after Restart now the old apitool may still be exiting, and
/// Windows won't delete a running exe (seen on the VM, the new one up in 84 ms).
pub fn clean_up() {
    let Ok(dir) = here() else { return };
    std::thread::spawn(move || {
        for _ in 0..20 {
            let olds = BINARIES.map(|b| dir.join(format!("{}.old", name(b))));
            if olds
                .iter()
                .all(|old| !old.exists() || std::fs::remove_file(old).is_ok())
            {
                return;
            }
            std::thread::sleep(Duration::from_millis(250));
        }
    });
}

/// Starts the binary now in place of this one, with the same arguments.
pub fn restart() -> Result<(), String> {
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let args = std::env::args_os().skip(1);
    std::process::Command::new(exe)
        .args(args)
        .spawn()
        .map(drop)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_by_number_not_by_text() {
        let [major, minor, patch] = numbers(CURRENT);
        assert!(is_newer(&format!("{major}.{minor}.{}", patch + 1)));
        assert!(is_newer(&format!("{major}.{}.0", minor + 10)), "10 after 9");
        assert!(is_newer(&format!("{}.0.0", major + 1)));
        assert!(!is_newer(CURRENT));
        assert!(
            !is_newer(&format!("{major}.{minor}")),
            "{major}.{minor} is older"
        );
    }

    fn numbers(v: &str) -> [u64; 3] {
        let n: Vec<u64> = v.split('.').map(|n| n.parse().unwrap()).collect();
        [n[0], n[1], n[2]]
    }

    #[test]
    fn the_archive_is_the_one_built_for_this_platform() {
        let asset = |target: &str| {
            let name = format!("apitool-v9.0.0-{target}.zip");
            format!(r#"{{"name": "{name}", "browser_download_url": "https://dl/{name}"}}"#)
        };
        let json = |assets: &[String]| {
            let assets = assets.join(",");
            format!(r#"{{"tag_name": "v9.0.0", "html_url": "https://page", "assets": [{assets}]}}"#)
        };
        let other = if TARGET.contains("windows") {
            "x86_64-apple-darwin"
        } else {
            "x86_64-pc-windows-msvc"
        };
        let release = parse(&json(&[asset(other), asset(TARGET)])).unwrap();
        assert_eq!(release.version, "9.0.0");
        assert_eq!(release.page, "https://page");
        assert_eq!(
            release.archive,
            Some(format!("https://dl/apitool-v9.0.0-{TARGET}.zip"))
        );
        assert_eq!(parse(&json(&[asset(other)])).unwrap().archive, None);
    }

    #[test]
    fn automatic_checks_follow_the_chosen_interval() {
        let day = 24 * 60 * 60;
        let at = |check, checked| Updates {
            check,
            install: false,
            checked,
        };
        assert!(!at(Check::Never, 0).due(10 * day, false));
        assert!(at(Check::AtStart, 10 * day).due(10 * day, false));
        assert!(!at(Check::AtStart, 0).due(10 * day, true), "once per run");
        assert!(!at(Check::Daily, 10 * day).due(11 * day - 1, false));
        assert!(at(Check::Daily, 10 * day).due(11 * day, false));
        assert!(!at(Check::Weekly, 10 * day).due(16 * day, false));
        assert!(at(Check::Weekly, 10 * day).due(17 * day, true));
    }

    /// The real archive's way in: packed by tar like the release workflow does, unpacked by
    /// the system's tar, swapped with the binaries in place, and the old ones cleared.
    #[test]
    fn an_archive_unpacks_and_takes_the_binaries_place() {
        let root = std::env::temp_dir().join(format!("apitool-update-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (built, work, installed) = (root.join("built"), root.join("work"), root.join("bin"));
        for dir in [&built, &work, &installed] {
            std::fs::create_dir_all(dir).unwrap();
        }
        for binary in BINARIES {
            std::fs::write(built.join(name(binary)), "new").unwrap();
            std::fs::write(installed.join(name(binary)), "old").unwrap();
        }
        let archive = root.join(if cfg!(windows) { "a.zip" } else { "a.tar.gz" });
        let packed = std::process::Command::new("tar")
            // -a packs by the suffix: on the Windows runner a zip, as the release has.
            .arg("-acf")
            .arg(&archive)
            .arg("-C")
            .arg(&built)
            .args(BINARIES.map(name))
            .status()
            .unwrap();
        assert!(packed.success());

        unpack(&archive, &work).unwrap();
        swap(&work, &installed).unwrap();
        let read = |file: String| std::fs::read_to_string(installed.join(file)).unwrap();
        for binary in BINARIES {
            assert_eq!(read(name(binary)), "new");
            assert_eq!(read(format!("{}.old", name(binary))), "old");
        }
        // An archive without the binaries changes nothing.
        assert!(swap(&root.join("built-not"), &installed).is_err());
        assert_eq!(read(name("apitool")), "new");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// The real thing, with the public tests (`--ignored`): GitHub's API, the redirect to
    /// its download host, this platform's archive and the system's tar.
    #[tokio::test]
    #[ignore]
    async fn public_the_latest_release_installs_into_a_folder() {
        crate::net::install_provider();
        let client = reqwest::Client::new();
        let release = latest(&client).await.unwrap();
        assert!(release.archive.is_some(), "{release:?}");
        let dir = std::env::temp_dir().join(format!("apitool-install-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        install_into(&client, &release, dir.clone()).await.unwrap();
        for binary in BINARIES {
            let size = std::fs::metadata(dir.join(name(binary))).unwrap().len();
            assert!(size > 1 << 20, "{binary}: {size} bytes");
        }
        assert!(
            !dir.join(".apitool-update").exists(),
            "the download is cleared"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
