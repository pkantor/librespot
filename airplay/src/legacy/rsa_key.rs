//! The classic AirPlay/RAOP RSA private key. This is not a secret this crate holds on the
//! receiver's behalf — it's the same fixed, long-published (leaked/reverse-engineered years ago)
//! key every open RAOP receiver implementation embeds, sourced from shairport-sync's own
//! `common.c` (`super_secret_key`, fetched and read directly). A real sender encrypts the
//! per-session AES key against this key's *public* half; any receiver holding the same private
//! key can decrypt it, which is the whole (weak, by design — this predates AirPlay 2's real
//! cryptography) point of the scheme.
//!
//! The base64 body below is the *same key material*, re-wrapped to standard 64-column PEM lines
//! (RFC 7468) rather than shairport-sync's own 76-column wrapping — confirmed by diffing the
//! decoded base64 payload byte-for-byte against the source before rewrapping it. OpenSSL/mbedTLS
//! (what shairport-sync actually links against) don't care about line width; the `pkcs1`/`pem`
//! crates this crate uses do, and reject 76-column lines as malformed rather than silently
//! accepting them — a real, confirmed incompatibility, not a stylistic choice.

use rsa::{Oaep, Pkcs1v15Sign, RsaPrivateKey, pkcs1::DecodeRsaPrivateKey};
use sha1::Sha1;

const PEM: &str = "-----BEGIN RSA PRIVATE KEY-----
MIIEpQIBAAKCAQEA59dE8qLieItsH1WgjrcFRKj6eUWqi+bGLOX1HL3U3GhC/j0Q
g90u3sG/1CUtwC5vOYvfDmFI6oSFXi5ELabWJmT2dKHzBJKa3k9ok+8t9ucRqMd6
DZHJ2YCCLlDRKSKv6kDqnw4UwPdpOMXziC/AMj3Z/lUVX1G7WSHCAWKf1zNS1eLv
qr+boEjXuBOitnZ/bDzPHrTOZz0Dew0uowxf/+sG+NCK3eQJVxqcaJ/vEHKIVd2M
+5qL71yJQ+87X6oV3eaYvt3zWZYD6z5vYTcrtij2VZ9Zmni/UAaHqn9JdsBWLUEp
VviYnhimNVvYFZeCXg/IdTQ+x4IRdiXNv5hEewIDAQABAoIBAQDl8Axy9XfWBLmk
zkEiqoSwF0PsmVrPzH9KsnwLGH+QZlvjWd8SWYGN7u1507HvhF5N3drJoVU3O14n
DY4TFQAaLlJ9VM35AApXaLyY1ERrN7u9ALKd2LUwYhM7Km539O4yUFYikE2nIPsc
EsA5ltpxOgUGCY7b7ez5NtD6nL1ZKauw7aNXmVAvmJTcuPxWmoktF3gDJKK2wxZu
NGcJE0uFQEG4Z3BrWP7yoNuSK3dii2jmlpPHr0O/KnPQtzI3eguhe0TwUem/eYSd
yzMyVx/YpwkzwtYL3sR5k0o9rKQLtvLzfAqdBxBurcizaaA/L0HIgAmOit1GJA2s
aMxTVPNhAoGBAPfgv1oeZxgxmotiCcMXFEQEWflzhWYTsXrhUIuz5jFua39GLS99
ZEErhLdrwj8rDDViRVJ5skOp9zFvlYAHs0xh92ji1E7V/ysnKBfsMrPkk5KSKPrn
jndMoPdevWnVkgJ5jxFuNgxkOLMuG9i53B4yMvDTCRiIPMQ++N2iLDaRAoGBAO9v
//mU8eVkQaoANf0ZoMjW8CN4xwWA2cSEIHkd9AfFkftuv8oyLDCG3ZAf0vrhrrtk
rfa7ef+AUb69DNggq4mHQAYBp7L+k5DKzJrKuO0r+R0YbY9pZD1+/g9dVt91d6LQ
NepUE/yY2PP5CNoFmjedpLHMOPFdVgqDzDFxU8hLAoGBANDrr7xAJbqBjHVwIzQ4
To9pb4BNeqDndk5Qe7fT3+/H1njGaC0/rXE0Qb7q5ySgnsCb3DvAcJyRM9SJ7OKl
Gt0FMSdJD5KG0XPIpAVNwgpXXH5MDJg09KHeh0kXo+QA6viFBi21y340NonnEfdf
54PX4ZGS/Xac1UK+pLkBB+zRAoGAf0AY3H3qKS2lMEI4bzEFoHeK3G895pDaK3TF
BVmD7fV0Zhov17fegFPMwOII8MisYm9ZfT2Z0s5Ro3s5rkt+nvLAdfC/PYPKzTLa
lpGSwomSNYJcB9HNMlmhkGzc1JnLYT4iyUyx6pcZBmCd8bD0iwY/FzcgNDaUmbX9
+XDvRA0CgYEAkE7pIPlE71qvfJQgoA9em0gILAuE4Pu13aKiJnfft7hIjbK+5kyb
3TysZvoyDnb3HOKvInK7vXbKuU4ISgxB2bB3HcYzQMGsz1qJ2gG0N5hvJpzwwhbh
XqFKA4zaaSrw622wDniAK5MlIE0tIAKKP4yxNGjoD2QYjhBGuhvkWKY=
-----END RSA PRIVATE KEY-----";

#[derive(Debug, thiserror::Error)]
pub(crate) enum RsaKeyError {
    #[error("failed to parse the embedded RAOP RSA private key: {0}")]
    Parse(#[from] rsa::pkcs1::Error),
    #[error("RSA-OAEP decryption failed: {0}")]
    Decrypt(#[from] rsa::Error),
    #[error("decrypted AES key was {0} bytes, expected 16")]
    WrongLength(usize),
}

/// Decrypts a `rsaaeskey` value from an `ANNOUNCE` SDP body (base64-decoded first by the caller)
/// into the 16-byte AES-128 key it encrypts. shairport-sync's own `rsa_apply(..., RSA_MODE_KEY)`
/// uses `RSA_PKCS1_OAEP_PADDING` with OpenSSL's default OAEP digest, which is SHA-1 — not
/// specified explicitly in its own source because it's relying on that default, so this crate
/// makes the same choice explicit rather than guessing a "more modern" hash that would silently
/// fail to interoperate.
pub(crate) fn decrypt_aes_key(encrypted: &[u8]) -> Result<[u8; 16], RsaKeyError> {
    let private_key = RsaPrivateKey::from_pkcs1_pem(PEM)?;
    let decrypted = private_key.decrypt(Oaep::new::<Sha1>(), encrypted)?;
    decrypted
        .try_into()
        .map_err(|v: Vec<u8>| RsaKeyError::WrongLength(v.len()))
}

/// Signs an `Apple-Challenge` buffer with the same embedded key, for the `Apple-Response` header
/// a classic sender demands before it will stream (see `rtsp::apple_challenge`).
///
/// PKCS#1 v1.5 over the buffer **with no digest at all** — not "sign the SHA-256 of it". That is
/// what shairport-sync's `rsa_apply(..., RSA_MODE_AUTH)` does in each of its three crypto
/// backends: mbedTLS passes `MBEDTLS_MD_NONE`, and OpenSSL calls `EVP_PKEY_sign` on the raw input
/// with `RSA_PKCS1_PADDING`. `Pkcs1v15Sign::new_unprefixed()` is the same thing here — the
/// digest-carrying variant would prepend a DigestInfo header and produce a signature no sender
/// accepts.
pub(crate) fn sign_challenge(data: &[u8]) -> Result<Vec<u8>, RsaKeyError> {
    let private_key = RsaPrivateKey::from_pkcs1_pem(PEM)?;
    Ok(private_key.sign(Pkcs1v15Sign::new_unprefixed(), data)?)
}

/// The public half of the embedded key, exposed only for `legacy`'s own integration test to
/// encrypt a test value against — a real sender does the equivalent using Apple's copy of the
/// same public key.
#[cfg(test)]
pub(crate) fn embedded_public_key_for_test() -> rsa::RsaPublicKey {
    rsa::RsaPublicKey::from(&RsaPrivateKey::from_pkcs1_pem(PEM).unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_pem_parses() {
        RsaPrivateKey::from_pkcs1_pem(PEM).expect("the embedded RAOP key must parse");
    }

    /// Round-trips a fresh 16-byte key through this key's own *public* half (independent of
    /// `decrypt_aes_key`'s OAEP parameters — this just proves the embedded PEM is really this
    /// key's private half by successfully decrypting something encrypted against its public half)
    /// and confirms `decrypt_aes_key` recovers exactly what was encrypted.
    #[test]
    fn decrypts_a_value_encrypted_against_its_own_public_key() {
        let private_key = RsaPrivateKey::from_pkcs1_pem(PEM).unwrap();
        let public_key = rsa::RsaPublicKey::from(&private_key);

        let aes_key = [0x42u8; 16];
        let mut rng = rand_core::OsRng;
        let encrypted = public_key
            .encrypt(&mut rng, Oaep::new::<Sha1>(), &aes_key)
            .unwrap();

        let decrypted = decrypt_aes_key(&encrypted).unwrap();
        assert_eq!(decrypted, aes_key);
    }

    #[test]
    fn rejects_garbage_ciphertext() {
        assert!(decrypt_aes_key(&[0u8; 256]).is_err());
    }
}
