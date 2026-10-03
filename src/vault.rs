//! Environment secrets are sealed with the OS before they reach apitool.db, so a copied,
//! synced or backed-up database doesn't give them away. Windows uses DPAPI (bound to the
//! signed-in user, nothing to keep); macOS AES-256-GCM with a key in the login keychain.
//! Elsewhere they stay plain: there is no OS store to lean on without a D-Bus stack.
//! What can't be unsealed is an error, never an empty list that a save would then write.

pub const PREFIX: &str = "sealed:";

/// `plain` as stored: sealed where the OS can, else as it is.
pub fn seal(plain: &str) -> Result<String, String> {
    use base64::Engine as _;
    match os::seal(plain.as_bytes())? {
        Some(blob) => Ok(format!(
            "{PREFIX}{}",
            base64::engine::general_purpose::STANDARD.encode(blob)
        )),
        None => Ok(plain.to_owned()),
    }
}

/// What `seal` stored; plain text (from before sealing, or another OS) passes through.
pub fn open(stored: &str) -> Result<String, String> {
    use base64::Engine as _;
    let Some(b64) = stored.strip_prefix(PREFIX) else {
        return Ok(stored.to_owned());
    };
    let blob =
        (base64::engine::general_purpose::STANDARD.decode(b64)).map_err(|e| e.to_string())?;
    let plain = os::open(&blob)?;
    String::from_utf8(plain).map_err(|e| e.to_string())
}

#[cfg(windows)]
mod os {
    use std::ptr::{null, null_mut};

    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData, CryptUnprotectData,
    };

    pub fn seal(plain: &[u8]) -> Result<Option<Vec<u8>>, String> {
        dpapi(plain, true).map(Some)
    }

    pub fn open(blob: &[u8]) -> Result<Vec<u8>, String> {
        dpapi(blob, false).map_err(|e| format!("{e} (sealed by another Windows user or machine?)"))
    }

    fn dpapi(data: &[u8], protect: bool) -> Result<Vec<u8>, String> {
        let input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(data.len()).map_err(|e| e.to_string())?,
            pbData: data.as_ptr().cast_mut(),
        };
        let mut out = CRYPT_INTEGER_BLOB {
            cbData: 0,
            pbData: null_mut(),
        };
        // SAFETY: `input` points at `data` for the call, which only reads it; `out` is
        // filled by the OS with a LocalAlloc'd buffer, copied and freed below.
        let ok = unsafe {
            match protect {
                true => CryptProtectData(
                    &input,
                    null(),
                    null(),
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                ),
                false => CryptUnprotectData(
                    &input,
                    null_mut(),
                    null(),
                    null(),
                    null(),
                    CRYPTPROTECT_UI_FORBIDDEN,
                    &mut out,
                ),
            }
        };
        if ok == 0 {
            return Err(format!("DPAPI: {}", std::io::Error::last_os_error()));
        }
        // SAFETY: on success `out` holds `cbData` bytes at `pbData`, ours to free.
        let bytes = unsafe { std::slice::from_raw_parts(out.pbData, out.cbData as usize) };
        let bytes = bytes.to_vec();
        unsafe { LocalFree(out.pbData.cast()) };
        Ok(bytes)
    }
}

#[cfg(target_os = "macos")]
mod os {
    use std::sync::Mutex;

    use ring::aead::{AES_256_GCM, Aad, LessSafeKey, NONCE_LEN, Nonce, UnboundKey};

    /// Read once per run. Only a key that was found or made is kept, so a locked keychain
    /// can be unlocked and tried again.
    static KEY: Mutex<Option<[u8; 32]>> = Mutex::new(None);

    pub fn seal(plain: &[u8]) -> Result<Option<Vec<u8>>, String> {
        let key = cipher()?;
        let mut nonce = [0u8; NONCE_LEN];
        getrandom::fill(&mut nonce).map_err(|e| e.to_string())?;
        let mut sealed = plain.to_vec();
        (key.seal_in_place_append_tag(
            Nonce::assume_unique_for_key(nonce),
            Aad::empty(),
            &mut sealed,
        ))
        .map_err(|_| "sealing failed".to_owned())?;
        Ok(Some([&nonce[..], &sealed].concat()))
    }

    pub fn open(blob: &[u8]) -> Result<Vec<u8>, String> {
        let key = cipher()?;
        if blob.len() < NONCE_LEN {
            return Err("sealed value is cut short".into());
        }
        let (nonce, sealed) = blob.split_at(NONCE_LEN);
        let nonce = Nonce::try_assume_unique_for_key(nonce).map_err(|_| "bad nonce")?;
        let mut sealed = sealed.to_vec();
        let plain = (key.open_in_place(nonce, Aad::empty(), &mut sealed)).map_err(|_| {
            "the keychain's apitool key doesn't open it (sealed on another Mac or user?)".to_owned()
        })?;
        Ok(plain.to_vec())
    }

    fn cipher() -> Result<LessSafeKey, String> {
        let mut cached = KEY.lock().unwrap_or_else(|e| e.into_inner());
        let key = match *cached {
            Some(k) => k,
            None => *cached.insert(keychain_key()?),
        };
        let key = UnboundKey::new(&AES_256_GCM, &key).map_err(|_| "bad key")?;
        Ok(LessSafeKey::new(key))
    }

    /// Tests keep their own item, so they never touch the app's key.
    const SERVICE: &str = if cfg!(test) {
        "apitool-test"
    } else {
        "apitool"
    };
    const ACCOUNT: &str = "secrets";

    /// Through Apple's `security` tool rather than the Keychain API: the item's access list
    /// then names that tool, so an unsigned apitool isn't asked about after each update.
    /// ponytail: the new key is on `security`'s command line for the instant it runs,
    /// visible to this user's own processes, which could read the database anyway.
    fn keychain_key() -> Result<[u8; 32], String> {
        let service = SERVICE;
        let find = || {
            std::process::Command::new("/usr/bin/security")
                .args(["find-generic-password", "-s", service, "-a", ACCOUNT, "-w"])
                .output()
        };
        let parse = |out: &[u8]| -> Option<[u8; 32]> {
            let hex = std::str::from_utf8(out).ok()?.trim();
            let bytes: Vec<u8> = (0..hex.len())
                .step_by(2)
                .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
                .collect::<Option<_>>()?;
            bytes.try_into().ok()
        };
        let found = find().map_err(|e| format!("keychain: {e}"))?;
        if found.status.success() {
            return parse(&found.stdout)
                .ok_or_else(|| "keychain: the apitool key is malformed".into());
        }
        let mut key = [0u8; 32];
        getrandom::fill(&mut key).map_err(|e| e.to_string())?;
        let hex: String = key.iter().map(|b| format!("{b:02x}")).collect();
        let added = std::process::Command::new("/usr/bin/security")
            .args([
                "add-generic-password",
                "-s",
                service,
                "-a",
                ACCOUNT,
                "-w",
                &hex,
            ])
            .output()
            .map_err(|e| format!("keychain: {e}"))?;
        if added.status.success() {
            return Ok(key);
        }
        // Another apitool may have made it first.
        let found = find().map_err(|e| format!("keychain: {e}"))?;
        match found.status.success() {
            true => {
                parse(&found.stdout).ok_or_else(|| "keychain: the apitool key is malformed".into())
            }
            false => Err(format!(
                "keychain: {}",
                String::from_utf8_lossy(&added.stderr).trim()
            )),
        }
    }
}

#[cfg(not(any(windows, target_os = "macos")))]
mod os {
    pub fn seal(_: &[u8]) -> Result<Option<Vec<u8>>, String> {
        Ok(None)
    }

    pub fn open(_: &[u8]) -> Result<Vec<u8>, String> {
        Err("sealed on Windows or macOS; this system can't open it".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn secrets_are_sealed_where_the_os_can_and_come_back() {
        let plain = r#"[{"key":"token","value":"s3cret","enabled":true}]"#;
        let stored = seal(plain).unwrap();
        if cfg!(any(windows, target_os = "macos")) {
            assert!(stored.starts_with(PREFIX), "{stored}");
            assert!(!stored.contains("s3cret"));
            // A fresh nonce (or DPAPI's salt) each time.
            assert_ne!(seal(plain).unwrap(), stored);
        } else {
            assert_eq!(stored, plain);
        }
        assert_eq!(open(&stored).unwrap(), plain);
        // From before sealing, read as it is.
        assert_eq!(open(plain).unwrap(), plain);
        // Tampered or foreign: an error, not garbage or nothing.
        if let Some(b64) = stored.strip_prefix(PREFIX) {
            use base64::Engine as _;
            let b64e = base64::engine::general_purpose::STANDARD;
            let mut blob = b64e.decode(b64).unwrap();
            *blob.last_mut().unwrap() ^= 1;
            assert!(open(&format!("{PREFIX}{}", b64e.encode(blob))).is_err());
        }
    }
}
