//! JWT bearer auth: the token is signed on every send, so `{{$timestamp}}` in the claims
//! is fresh each time. ring (already in for rustls) does every algorithm.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use ring::rand::SystemRandom;
use ring::{hmac, signature};
use rustls::pki_types::PrivateKeyDer;
use rustls::pki_types::pem::PemObject;

use crate::model::Jwt;

pub const ALGORITHMS: [&str; 12] = [
    "HS256", "HS384", "HS512", "RS256", "RS384", "RS512", "PS256", "PS384", "PS512", "ES256",
    "ES384", "EdDSA",
];

/// The compact token for an already-resolved `Jwt`.
pub fn sign(j: &Jwt) -> Result<String, String> {
    let claims = match j.payload.trim() {
        "" => "{}",
        p => p,
    };
    let claims: serde_json::Value =
        serde_json::from_str(claims).map_err(|e| format!("JWT payload: {e}"))?;
    if !claims.is_object() {
        return Err("JWT payload: must be a JSON object of claims".into());
    }
    let header = serde_json::json!({ "alg": j.algorithm, "typ": "JWT" });
    let input = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(header.to_string()),
        URL_SAFE_NO_PAD.encode(claims.to_string())
    );
    let sig = signature_of(&j.algorithm, &j.secret, input.as_bytes())?;
    Ok(format!("{input}.{}", URL_SAFE_NO_PAD.encode(sig)))
}

pub(crate) fn signature_of(alg: &str, secret: &str, msg: &[u8]) -> Result<Vec<u8>, String> {
    let rng = SystemRandom::new();
    let hs = |a| {
        Ok(hmac::sign(&hmac::Key::new(a, secret.as_bytes()), msg)
            .as_ref()
            .to_vec())
    };
    let bad = |e: ring::error::KeyRejected| format!("JWT {alg} private key: {e}");
    let rsa = |pad: &'static dyn signature::RsaEncoding| {
        let key = match key(alg, secret)? {
            PrivateKeyDer::Pkcs8(k) => signature::RsaKeyPair::from_pkcs8(k.secret_pkcs8_der()),
            PrivateKeyDer::Pkcs1(k) => signature::RsaKeyPair::from_der(k.secret_pkcs1_der()),
            _ => return Err(format!("JWT {alg}: the private key isn't an RSA key")),
        }
        .map_err(bad)?;
        let mut sig = vec![0; key.public().modulus_len()];
        (key.sign(pad, &rng, msg, &mut sig)).map_err(|_| format!("JWT {alg}: signing failed"))?;
        Ok(sig)
    };
    let pkcs8 = || match key(alg, secret)? {
        PrivateKeyDer::Pkcs8(k) => Ok(k),
        // ring reads EC keys only as PKCS#8.
        _ => Err(format!(
            "JWT {alg}: the private key must be PKCS#8 (BEGIN PRIVATE KEY); convert it with \
             openssl pkcs8 -topk8 -nocrypt"
        )),
    };
    let es = |a| {
        let key = signature::EcdsaKeyPair::from_pkcs8(a, pkcs8()?.secret_pkcs8_der(), &rng)
            .map_err(bad)?;
        let sig = key
            .sign(&rng, msg)
            .map_err(|_| format!("JWT {alg}: signing failed"))?;
        Ok(sig.as_ref().to_vec())
    };
    match alg {
        "HS256" => hs(hmac::HMAC_SHA256),
        "HS384" => hs(hmac::HMAC_SHA384),
        "HS512" => hs(hmac::HMAC_SHA512),
        "RS256" => rsa(&signature::RSA_PKCS1_SHA256),
        "RS384" => rsa(&signature::RSA_PKCS1_SHA384),
        "RS512" => rsa(&signature::RSA_PKCS1_SHA512),
        "PS256" => rsa(&signature::RSA_PSS_SHA256),
        "PS384" => rsa(&signature::RSA_PSS_SHA384),
        "PS512" => rsa(&signature::RSA_PSS_SHA512),
        "ES256" => es(&signature::ECDSA_P256_SHA256_FIXED_SIGNING),
        "ES384" => es(&signature::ECDSA_P384_SHA384_FIXED_SIGNING),
        "EdDSA" => {
            let key =
                signature::Ed25519KeyPair::from_pkcs8_maybe_unchecked(pkcs8()?.secret_pkcs8_der())
                    .map_err(bad)?;
            Ok(key.sign(msg).as_ref().to_vec())
        }
        other => Err(format!("JWT algorithm \"{other}\" isn't supported")),
    }
}

fn key(alg: &str, pem: &str) -> Result<PrivateKeyDer<'static>, String> {
    PrivateKeyDer::from_pem_slice(pem.trim().as_bytes())
        .map_err(|e| format!("JWT {alg} needs a PEM private key as the secret: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    const RSA: &str = "-----BEGIN PRIVATE KEY-----
MIIEvgIBADANBgkqhkiG9w0BAQEFAASCBKgwggSkAgEAAoIBAQC9GeFV9hnoeRpv
SjeiKlgHZ+jmV4HqPhecUN0UEBAkWeyIJEqNW1LKQsxkgIYY4sVO7q3J5WTMRyUo
OBrGiufxWB58442EYvaFK+mIOzSHPHit26FudB9KRlpdR0aDjUsq8NZgIMz1Gequ
cTX0f0mBfJQU7bF4qDUdzqnBR8zD6IygiQDzv6XJ5Yy9H2GWgEzgNaH6kY68k+vK
/5l8AUPNyrw6XgdlaCZltrWHChssYPFveaFtR1TrBT+B/npJbA+1k8j/R1eDQCIU
+z01YE9wavg1+CFIsVivsJIVsM5iWrO4kIprVgWyUMnp7bHUxGMkegYSQ1sqjJZQ
l1UMzh1BAgMBAAECggEAIC31xyuUmBd3tKWUFxAOn+ACZaRRktuTKAIoxP/Ax3bY
Bgjq+OgoDAxW/OlUKIr6maaLQ3a6cvrOa2w0vkGoG81rjsQocnVmzx28ZXbxxuu2
+5sK+yFeq8SSHxqAeOWD+6A1UvFx/2m0IpBYZq18hED/cBpM36P8OgDPqXj+8v1l
0mKYPXxepbweJF6be06LTpu81IX6M0T31rrGyKb7Idlyqqwox1x+dZbTb6bzJU9r
LkwvzHGF3wRz1j8XeFluQzcfB3q/sS2KGq27Rh7oiGaKXIQXirsYO6jEoaU4+Ibs
FtoBhwa+vxP6DSD6/g+l7IxC/YPV8dzw3UvMZyHHowKBgQD78tfthKBVfFPTFt45
U1JJsGi+Ntht0IrNJV4CWkieMrnHw8EZeIiMkJNhSTj/x5R3kr01ACB8+YsqqQmy
YGznUI7I69erpSGlF/0JaCM+HO2xT3gBwZ3+2PvI2skjq+8E8EW7HrNbs5seh05K
B42fDKKj2xmQtsO7+u1r+yBgRwKBgQDAJFKL4syJZmCAifgHFWtbEIMpSafTxxwF
L5i31/CACHob7kpOSawVauvHUweGeHJA6y1tIo3Atl+D0/E3zZ1BGg2FtAT8hRuT
+OBc0yUoNGVUU9V0hU4RwXkns4i/Ueq10rkNLxYzAL9acf1vjy0aPKIVGuA2DAKL
NH2ga3AiNwKBgQCPArkuSSn5XCj4mPJq97Ctw0SxM9CGBOnEqIFENJsjsQdjLOpe
2twnbak6f6WrCk5r0Q81Fm1agwtLm8e1SKaIZmGmCrjQ5VrDq2ol/MaEa0dAbitg
U9aq4d+Jkya46M8zrm7mV/bXBov2ODdoLgFlVna7K5LHYfaYrUY3FMS74QKBgG2q
zaqmEpRB6Ma0+OoiIZpifFpufen0dVvIZORZzh1luTyD78lrZ1r6IgUssNjhmmTP
Vqg51qqt7SpzJ/Tv2Ne1pQ4xR79RwgHdRUH2Cfk+nq9ZAjZ1d6/Ou/YbFOwON2b8
FT8fJw6JWK6o7TxlfhrBjMl7A4oVpMYLecC8Uc5VAoGBAPIfkhPvnUfV7a1/qZE4
sbhVHZL60z1osvd+TXk0c0WXkAoiqDD72xdwsciMV+NPzUnCvKcRJmleq+z3P5wE
SxklGjhyrJvY1HxGpYJRu5H9Mo+woo+PuU8jNRlIa/i7zG2DQ1SCO3MM6vWVdKmc
HVbhlnvmhVqnjypOaSnrvscI
-----END PRIVATE KEY-----\n";

    fn jwt(algorithm: &str, secret: &str, payload: &str) -> Jwt {
        Jwt {
            algorithm: algorithm.into(),
            secret: secret.into(),
            payload: payload.into(),
        }
    }

    /// The signed part and the signature, decoded.
    fn split(token: &str) -> (String, Vec<u8>) {
        let (input, sig) = token.rsplit_once('.').unwrap();
        (input.to_owned(), URL_SAFE_NO_PAD.decode(sig).unwrap())
    }

    #[test]
    fn hs256_matches_the_jwt_io_example() {
        let claims = r#"{"sub":"1234567890","name":"John Doe","iat":1516239022}"#;
        assert_eq!(
            sign(&jwt("HS256", "your-256-bit-secret", claims)).unwrap(),
            "eyJhbGciOiJIUzI1NiIsInR5cCI6IkpXVCJ9.\
             eyJzdWIiOiIxMjM0NTY3ODkwIiwibmFtZSI6IkpvaG4gRG9lIiwiaWF0IjoxNTE2MjM5MDIyfQ.\
             SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
        );
    }

    #[test]
    fn asymmetric_tokens_verify_with_the_public_key() {
        let verify = |alg: &str,
                      pem: &str,
                      public: &[u8],
                      v: &'static dyn signature::VerificationAlgorithm| {
            let token = sign(&jwt(alg, pem, r#"{"sub":"ada"}"#)).unwrap();
            let header = token.split('.').next().unwrap();
            let header: serde_json::Value =
                serde_json::from_slice(&URL_SAFE_NO_PAD.decode(header).unwrap()).unwrap();
            assert_eq!(header["alg"], alg);
            let (input, sig) = split(&token);
            signature::UnparsedPublicKey::new(v, public)
                .verify(input.as_bytes(), &sig)
                .unwrap_or_else(|_| panic!("{alg} doesn't verify"));
        };
        let rsa = PrivateKeyDer::from_pem_slice(RSA.as_bytes()).unwrap();
        let rsa = signature::RsaKeyPair::from_pkcs8(rsa.secret_der()).unwrap();
        let rsa_public = rsa.public().as_ref();
        verify(
            "RS256",
            RSA,
            rsa_public,
            &signature::RSA_PKCS1_2048_8192_SHA256,
        );
        verify(
            "RS512",
            RSA,
            rsa_public,
            &signature::RSA_PKCS1_2048_8192_SHA512,
        );
        verify(
            "PS256",
            RSA,
            rsa_public,
            &signature::RSA_PSS_2048_8192_SHA256,
        );
        for (alg, kind, v) in [
            (
                "ES256",
                &rcgen::PKCS_ECDSA_P256_SHA256,
                &signature::ECDSA_P256_SHA256_FIXED as &dyn signature::VerificationAlgorithm,
            ),
            (
                "ES384",
                &rcgen::PKCS_ECDSA_P384_SHA384,
                &signature::ECDSA_P384_SHA384_FIXED,
            ),
            ("EdDSA", &rcgen::PKCS_ED25519, &signature::ED25519),
        ] {
            let key = rcgen::KeyPair::generate_for(kind).unwrap();
            verify(alg, &key.serialize_pem(), key.public_key_raw(), v);
        }
    }

    #[test]
    fn mistakes_say_what_to_fix() {
        let err = |j: Jwt| sign(&j).unwrap_err();
        assert!(err(jwt("HS256", "s", "{ nope")).contains("JWT payload"));
        assert!(err(jwt("HS256", "s", "[1]")).contains("JSON object"));
        assert!(err(jwt("none", "s", "{}")).contains("isn't supported"));
        assert!(err(jwt("RS256", "a shared secret", "{}")).contains("PEM private key"));
        let ec = rcgen::KeyPair::generate_for(&rcgen::PKCS_ECDSA_P256_SHA256).unwrap();
        assert!(err(jwt("RS256", &ec.serialize_pem(), "{}")).contains("RS256 private key"));
        // An empty payload is an empty claim set, not an error.
        assert!(sign(&jwt("HS256", "s", "  ")).is_ok());
    }
}
