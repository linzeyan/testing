//! NTLMv2 (MS-NLMP) for HTTP: the negotiate and authenticate messages. NTLM signs in a
//! connection, not a request: `http::execute` sends both on a client from
//! `net::Clients::ntlm_for`, which keeps them on one.

use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use hmac::{Hmac, KeyInit, Mac};

use crate::model::Ntlm;

const UNICODE: u32 = 0x1;
const OEM: u32 = 0x2;
const REQUEST_TARGET: u32 = 0x4;
const NTLM: u32 = 0x200;
const ALWAYS_SIGN: u32 = 0x8000;
const EXTENDED_SESSIONSECURITY: u32 = 0x8_0000;
const TARGET_INFO: u32 = 0x80_0000;
/// What the negotiate message offers: curl's set, plus Unicode.
const OFFERED: u32 = UNICODE | OEM | REQUEST_TARGET | NTLM | ALWAYS_SIGN | EXTENDED_SESSIONSECURITY;

const SIGNATURE: &[u8; 8] = b"NTLMSSP\0";

/// The `Authorization` value that opens the handshake. The domain and workstation go in
/// the authenticate message instead.
pub fn negotiate() -> String {
    let mut m = SIGNATURE.to_vec();
    m.extend(1u32.to_le_bytes());
    m.extend(OFFERED.to_le_bytes());
    m.extend([0; 16]);
    format!("NTLM {}", STANDARD.encode(m))
}

/// The `Authorization` value answering the server's challenge (what follows `NTLM `).
pub fn authenticate(n: &Ntlm, challenge: &str) -> Result<String, String> {
    let challenge = (STANDARD.decode(challenge.trim()))
        .map_err(|e| format!("NTLM: the server's challenge isn't base64: {e}"))?;
    let mut client = [0u8; 8];
    getrandom::fill(&mut client).map_err(|e| e.to_string())?;
    // A FILETIME: 100 ns ticks since 1601.
    let since_1601 = (SystemTime::now().duration_since(SystemTime::UNIX_EPOCH)).unwrap_or_default()
        + Duration::from_secs(11_644_473_600);
    let now = (since_1601.as_nanos() / 100) as u64;
    let message = authenticate_with(n, &challenge, now, client)?;
    Ok(format!("NTLM {}", STANDARD.encode(message)))
}

fn authenticate_with(
    n: &Ntlm,
    challenge: &[u8],
    now: u64,
    client: [u8; 8],
) -> Result<Vec<u8>, String> {
    let bad = || "NTLM: the server's challenge is malformed".to_owned();
    let u16_at = |i: usize| {
        challenge
            .get(i..i + 2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
    };
    let u32_at = |i: usize| {
        (challenge.get(i..i + 4)).map(|b| u32::from_le_bytes(b.try_into().expect("4 bytes")))
    };
    if !challenge.starts_with(SIGNATURE) || u32_at(8) != Some(2) {
        return Err(bad());
    }
    let flags = u32_at(20).ok_or_else(bad)?;
    let server = challenge.get(24..32).ok_or_else(bad)?;
    let target_info = match flags & TARGET_INFO {
        0 => &[][..],
        _ => {
            let (len, at) = (u16_at(40).ok_or_else(bad)?, u32_at(44).ok_or_else(bad)?);
            (challenge.get(at as usize..at as usize + len as usize)).ok_or_else(bad)?
        }
    };
    // The server's clock, which the spec says to use when it sends one (no skew to
    // worry about), and then to send no LMv2 response.
    let stamp = server_time(target_info);

    let (user, domain) = match n.domain.trim() {
        "" => split_username(n.username.trim()),
        domain => (n.username.trim(), domain),
    };
    let key = ntowfv2(&n.password, user, domain);
    let mut temp = vec![1, 1, 0, 0, 0, 0, 0, 0];
    temp.extend(stamp.unwrap_or(now).to_le_bytes());
    temp.extend(client);
    temp.extend([0; 4]);
    temp.extend(target_info);
    temp.extend([0; 4]);
    let proof = hmac_md5(&key, &[server, &temp].concat());
    let nt = [&proof[..], &temp].concat();
    let lm = match stamp {
        Some(_) => vec![0; 24],
        None => [&hmac_md5(&key, &[server, &client].concat())[..], &client].concat(),
    };

    let unicode = flags & UNICODE != 0;
    let text = |s: &str| match unicode {
        true => utf16le(s),
        false => s.as_bytes().to_vec(),
    };
    // The fields in the header's order; the last is the session key, which isn't exchanged.
    let fields = [
        lm,
        nt,
        text(domain),
        text(user),
        text(n.workstation.trim()),
        Vec::new(),
    ];
    let mut m = SIGNATURE.to_vec();
    m.extend(3u32.to_le_bytes());
    let mut at = (m.len() + 8 * fields.len() + 4) as u32;
    for f in &fields {
        m.extend((f.len() as u16).to_le_bytes());
        m.extend((f.len() as u16).to_le_bytes());
        m.extend(at.to_le_bytes());
        at += f.len() as u32;
    }
    let charset = if unicode { UNICODE } else { OEM };
    m.extend(((flags & OFFERED & !(UNICODE | OEM)) | charset).to_le_bytes());
    m.extend(fields.concat());
    Ok(m)
}

/// MsvAvTimestamp from the challenge's target info, if it has one.
fn server_time(mut info: &[u8]) -> Option<u64> {
    while info.len() >= 4 {
        let id = u16::from_le_bytes([info[0], info[1]]);
        let len = u16::from_le_bytes([info[2], info[3]]) as usize;
        let value = info.get(4..4 + len)?;
        match id {
            0 => return None,
            7 => return Some(u64::from_le_bytes(value.try_into().ok()?)),
            _ => info = &info[4 + len..],
        }
    }
    None
}

/// `DOMAIN\user` or `user@domain` when no domain is set, as Postman reads them; anything
/// else is the user name as typed.
fn split_username(name: &str) -> (&str, &str) {
    match (name.split_once('\\'), name.split_once('@')) {
        (Some((domain, user)), None) if !domain.is_empty() && !user.is_empty() => (user, domain),
        (None, Some((user, domain))) if !user.is_empty() && !domain.is_empty() => (user, domain),
        _ => (name, ""),
    }
}

/// NTOWFv2: the NT hash (MD4 of the UTF-16 password) keyed over the upper-cased user and
/// the domain as given.
fn ntowfv2(password: &str, user: &str, domain: &str) -> [u8; 16] {
    let who = [utf16le(&user.to_uppercase()), utf16le(domain)].concat();
    hmac_md5(&md4(&utf16le(password)), &who)
}

fn hmac_md5(key: &[u8], data: &[u8]) -> [u8; 16] {
    let mut mac = Hmac::<md5::Md5>::new_from_slice(key).expect("HMAC takes any key length");
    mac.update(data);
    mac.finalize().into_bytes().into()
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(u16::to_le_bytes).collect()
}

/// MD4 (RFC 1320), for the NT hash only. ponytail: written out (checked by the RFC's test
/// suite below) rather than a crate for one call on a short password.
fn md4(data: &[u8]) -> [u8; 16] {
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend((data.len() as u64).wrapping_mul(8).to_le_bytes());
    let mut state = [0x6745_2301u32, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476];
    for block in msg.chunks(64) {
        let x: Vec<u32> = (block.chunks(4))
            .map(|w| u32::from_le_bytes(w.try_into().expect("4 bytes")))
            .collect();
        // Each step updates the first word, then the words rotate so the next updates the
        // old last one: the RFC's [abcd k s], [dabc k s], [cdab k s], [bcda k s].
        let step = |r: &mut [u32; 4], f: u32, x: u32, s: u32| {
            let [a, b, c, d] = *r;
            *r = [d, a.wrapping_add(f).wrapping_add(x).rotate_left(s), b, c];
        };
        let mut r = state;
        for i in 0..16 {
            let [_, b, c, d] = r;
            step(&mut r, (b & c) | (!b & d), x[i], [3, 7, 11, 19][i % 4]);
        }
        for i in 0..16 {
            let [_, b, c, d] = r;
            let k = i % 4 * 4 + i / 4;
            let f = (b & c) | (b & d) | (c & d);
            step(
                &mut r,
                f,
                x[k].wrapping_add(0x5a82_7999),
                [3, 5, 9, 13][i % 4],
            );
        }
        for i in 0..16 {
            let [_, b, c, d] = r;
            let k = [0, 8, 4, 12, 2, 10, 6, 14, 1, 9, 5, 13, 3, 11, 7, 15][i];
            step(
                &mut r,
                b ^ c ^ d,
                x[k].wrapping_add(0x6ed9_eba1),
                [3, 9, 11, 15][i % 4],
            );
        }
        for (s, v) in state.iter_mut().zip(r) {
            *s = s.wrapping_add(v);
        }
    }
    let mut out = [0u8; 16];
    for (o, s) in out.chunks_mut(4).zip(state) {
        o.copy_from_slice(&s.to_le_bytes());
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use std::io::Write as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering::SeqCst};

    use super::*;
    use crate::model::{Auth, Body, Request};

    fn hex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len())
            .step_by(2)
            .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
            .collect()
    }

    /// The `i`th field of an authenticate message: LM, NT, domain, user, workstation.
    fn field(m: &[u8], i: usize) -> Vec<u8> {
        let at = 12 + 8 * i;
        let len = u16::from_le_bytes([m[at], m[at + 1]]) as usize;
        let offset = u32::from_le_bytes(m[at + 4..at + 8].try_into().unwrap()) as usize;
        m[offset..offset + len].to_vec()
    }

    #[test]
    fn md4_passes_the_rfc_1320_test_suite() {
        let suite = [
            ("", "31d6cfe0d16ae931b73c59d7e0c089c0"),
            ("a", "bde52cb31de33e46245e05fbdbd6fb24"),
            ("abc", "a448017aaf21d8525fc10ae87aa6729d"),
            ("message digest", "d9130a8164549fe818874806e1c7014b"),
            (
                "abcdefghijklmnopqrstuvwxyz",
                "d79e1c308aa5bbcdeea8ed63df412da9",
            ),
            (
                "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789",
                "043f8582f241db351ce627e153e7f0e4",
            ),
            (
                "12345678901234567890123456789012345678901234567890123456789012345678901234567890",
                "e33b4ddc9c38f2199c3e7b164fcc0536",
            ),
        ];
        for (input, digest) in suite {
            assert_eq!(md4(input.as_bytes()).to_vec(), hex(digest), "{input:?}");
        }
    }

    /// MS-NLMP §4.2.4.3's CHALLENGE_MESSAGE: server challenge 0123456789abcdef, and target
    /// info naming domain "Domain" and server "Server", with no timestamp.
    const CHALLENGE: &str = "
        4e544c4d53535000 020000000c000c00 3800000033828ae2 0123456789abcdef
        0000000000000000 2400240044000000 060070170000000f 530065007200760065007200
        02000c00 44006f006d00610069006e00 01000c00 530065007200760065007200 00000000";

    /// MS-NLMP §4.2.4 with §4.2.1's values (user "User", domain "Domain", password
    /// "Password", time zero, client challenge aa…): a server checks these bytes, so they
    /// must match the spec's to the bit.
    #[test]
    fn ntlmv2_matches_the_ms_nlmp_example() {
        assert_eq!(
            ntowfv2("Password", "User", "Domain").to_vec(),
            hex("0c868a403bfd7a93a3001ef22ef02e3f")
        );
        let n = Ntlm {
            username: "User".into(),
            password: "Password".into(),
            domain: "Domain".into(),
            workstation: "COMPUTER".into(),
        };
        let m = authenticate_with(&n, &hex(CHALLENGE), 0, [0xaa; 8]).unwrap();
        assert_eq!(
            field(&m, 0),
            hex("86c35097ac9cec102554764a57cccc19 aaaaaaaaaaaaaaaa"),
            "LMv2 (§4.2.4.2.1)"
        );
        // §4.2.4.2.2's NTProofStr, then temp (§4.2.4.1.3).
        let nt = "68cd0ab851e51c96aabc927bebef6a1c
                  0101000000000000 0000000000000000 aaaaaaaaaaaaaaaa 00000000
                  02000c0044006f006d00610069006e00 01000c00530065007200760065007200
                  0000000000000000";
        assert_eq!(field(&m, 1), hex(nt), "NTLMv2");
        let names = (field(&m, 2), field(&m, 3), field(&m, 4));
        let expected = (utf16le("Domain"), utf16le("User"), utf16le("COMPUTER"));
        assert_eq!(names, expected);

        // `DOMAIN\user`, as people type it on Windows, is the same sign-in.
        let typed = Ntlm {
            username: "Domain\\User".into(),
            domain: String::new(),
            ..n.clone()
        };
        assert_eq!(
            authenticate_with(&typed, &hex(CHALLENGE), 0, [0xaa; 8]),
            Ok(m)
        );

        // A server that sends its clock (MsvAvTimestamp) gets that time back, and no LMv2.
        let mut stamped = hex(CHALLENGE);
        let eol = stamped.len() - 4;
        let pair = [7, 0, 8, 0].into_iter().chain(42u64.to_le_bytes());
        stamped.splice(eol..eol, pair);
        (stamped[40], stamped[42]) = (stamped[40] + 12, stamped[42] + 12);
        let m = authenticate_with(&n, &stamped, 0, [0xaa; 8]).unwrap();
        assert_eq!(field(&m, 0), [0; 24]);
        assert_eq!(field(&m, 1)[24..32], 42u64.to_le_bytes());

        assert!(authenticate_with(&n, b"NTLMSSP\0", 0, [0; 8]).is_err());
    }

    /// What IIS sends with a 401 (abridged): a body means the connection goes back to the
    /// client's pool a moment after the response, the window a second one would open in.
    const PAGE: &str = "<!DOCTYPE html PUBLIC \"-//W3C//DTD XHTML 1.0 Strict//EN\">\
        <html><head><title>401 - Unauthorized: Access is denied due to invalid credentials.\
        </title></head><body><div id=\"content\"><h2>401 - Unauthorized: Access is denied due \
        to invalid credentials.</h2><h3>You do not have permission to view this directory or \
        page using the credentials that you supplied.</h3></div></body></html>";

    /// A stand-in for IIS: it answers a negotiate message with §4.2.4's challenge, and
    /// signs in an authenticate message only on the connection that got the challenge, and
    /// only if it proves the password. Returns its URL and how many connections it took.
    pub(crate) fn iis(password: &'static str) -> (String, Arc<AtomicUsize>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/app", listener.local_addr().unwrap());
        let opened = Arc::new(AtomicUsize::new(0));
        let count = opened.clone();
        std::thread::spawn(move || {
            for mut stream in listener.incoming().flatten() {
                count.fetch_add(1, SeqCst);
                std::thread::spawn(move || {
                    let mut challenged = false;
                    loop {
                        let req = crate::http::tests::read_request(&mut stream);
                        if req.is_empty() {
                            return;
                        }
                        let message = (req.lines())
                            .find_map(|l| l.strip_prefix("authorization: NTLM "))
                            .map(|b64| STANDARD.decode(b64.trim()).unwrap());
                        let (status, body) = match message {
                            Some(m) if m[8] == 1 => {
                                challenged = true;
                                let challenge = STANDARD.encode(hex(CHALLENGE));
                                let head = format!(
                                    "401 Unauthorized\r\nwww-authenticate: NTLM {challenge}"
                                );
                                (head, PAGE.to_owned())
                            }
                            Some(m) if m[8] == 3 && challenged && proves(&m, password) => {
                                let who = |i| String::from_utf16_lossy(&utf16(&field(&m, i)));
                                ("200 OK".into(), format!("{}\\{}", who(2), who(3)))
                            }
                            _ => (
                                "401 Unauthorized\r\nwww-authenticate: NTLM".into(),
                                PAGE.to_owned(),
                            ),
                        };
                        let head = format!(
                            "HTTP/1.1 {status}\r\ncontent-length: {}\r\n\r\n{body}",
                            body.len()
                        );
                        if stream.write_all(head.as_bytes()).is_err() {
                            return;
                        }
                    }
                });
            }
        });
        (url, opened)
    }

    fn utf16(bytes: &[u8]) -> Vec<u16> {
        (bytes.chunks(2))
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect()
    }

    /// The server's side of NTLMv2: the proof must come from the password.
    fn proves(m: &[u8], password: &str) -> bool {
        let (nt, domain, user) = (field(m, 1), field(m, 2), field(m, 3));
        let name = |b: &[u8]| String::from_utf16_lossy(&utf16(b));
        let key = ntowfv2(password, &name(&user), &name(&domain));
        let server = &hex(CHALLENGE)[24..32];
        nt.len() > 16 && hmac_md5(&key, &[server, &nt[16..]].concat())[..] == nt[..16]
    }

    fn clients(rt: &tokio::runtime::Runtime) -> crate::net::Clients {
        let net = crate::net::Network {
            proxy: crate::net::ProxyMode::None,
            ..Default::default()
        };
        rt.block_on(crate::net::build_client(net)).unwrap()
    }

    /// NTLM signs in the connection, so the authenticate message must go out on the one
    /// that got the challenge. Twenty rounds on a multi-threaded runtime, where hyper's
    /// pool sometimes opened a new connection for it.
    #[test]
    fn the_handshake_stays_on_one_connection() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        let (url, opened) = iis("Password");
        let clients = clients(&rt);
        let req = |password: &str| Request {
            method: "POST".into(),
            url: url.clone(),
            body: Body::Json { text: "{}".into() },
            auth: Auth::Ntlm(Ntlm {
                username: "CORP\\User".into(),
                password: password.into(),
                ..Default::default()
            }),
            ..Default::default()
        };
        for round in 1..=20 {
            let resp = rt
                .block_on(crate::runner::send(&clients, req("Password")))
                .unwrap();
            let got = (resp.status, resp.body.as_str());
            assert_eq!(got, (200, "CORP\\User"), "round {round}");
        }
        // One connection per handshake, none opened beside it.
        assert_eq!(opened.load(SeqCst), 20);
        let wrong = rt.block_on(crate::runner::send(&clients, req("guess")));
        assert_eq!(wrong.unwrap().status, 401);
    }
}
