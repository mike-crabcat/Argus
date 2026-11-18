//! Web Push without a third party: VAPID (RFC 8292) to identify Argus to
//! the browser's push service, and `aes128gcm` payload encryption
//! (RFC 8291) so only the subscribed browser can read a notification.
//! Pure Rust (RustCrypto P-256, HKDF-SHA-256, AES-128-GCM).

use std::path::Path;

use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::{Aes128Gcm, Nonce};
use anyhow::{Context, Result, anyhow, bail};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD as B64;
use hkdf::Hkdf;
use p256::ecdsa::signature::Signer;
use p256::ecdsa::{Signature, SigningKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use p256::{PublicKey, SecretKey};
use sha2::Sha256;

pub fn b64(bytes: &[u8]) -> String {
    B64.encode(bytes)
}

pub fn unb64(s: &str) -> Result<Vec<u8>> {
    // Browsers send unpadded base64url; tolerate padding anyway.
    B64.decode(s.trim_end_matches('='))
        .map_err(|e| anyhow!("bad base64url: {e}"))
}

fn random_secret() -> Result<SecretKey> {
    for _ in 0..8 {
        let mut b = [0u8; 32];
        getrandom::fill(&mut b).map_err(|e| anyhow!("no randomness: {e}"))?;
        if let Ok(k) = SecretKey::from_slice(&b) {
            return Ok(k);
        }
    }
    bail!("couldn't make a key")
}

/// The server's VAPID key pair.
pub struct Vapid {
    secret: SecretKey,
}

impl Vapid {
    /// Loads `vapid.key` from `root`, or makes one (chmod 600).
    pub fn load_or_create(root: &Path) -> Result<Self> {
        let path = root.join("vapid.key");
        match std::fs::read_to_string(&path) {
            Ok(s) => {
                let bytes = unb64(s.trim())?;
                let secret =
                    SecretKey::from_slice(&bytes).context("vapid.key is not a P-256 key")?;
                Ok(Vapid { secret })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let secret = random_secret()?;
                let tmp = path.with_extension("key.partial");
                std::fs::write(&tmp, b64(&secret.to_bytes()))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
                }
                std::fs::rename(&tmp, &path)?;
                tracing::info!("made a Web Push (VAPID) key");
                Ok(Vapid { secret })
            }
            Err(e) => Err(e).with_context(|| format!("reading {}", path.display())),
        }
    }

    pub fn from_secret(bytes: &[u8]) -> Result<Self> {
        Ok(Vapid {
            secret: SecretKey::from_slice(bytes)?,
        })
    }

    /// The public key, uncompressed, base64url: the browser's
    /// `applicationServerKey`.
    pub fn public_key(&self) -> String {
        b64(self.secret.public_key().to_encoded_point(false).as_bytes())
    }

    /// `Authorization: vapid t=..., k=...` for a push to `endpoint`.
    pub fn authorization(&self, endpoint: &str, subject: &str, now_s: i64) -> Result<String> {
        let url = url::Url::parse(endpoint)?;
        let aud = format!(
            "{}://{}",
            url.scheme(),
            url.host_str()
                .ok_or_else(|| anyhow!("endpoint has no host"))?
        ) + &url.port().map(|p| format!(":{p}")).unwrap_or_default();
        let header = b64(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = b64(serde_json::json!({
            "aud": aud,
            "exp": now_s + 12 * 3600,
            "sub": subject,
        })
        .to_string()
        .as_bytes());
        let signing_input = format!("{header}.{claims}");
        let sig: Signature = SigningKey::from(&self.secret).sign(signing_input.as_bytes());
        Ok(format!(
            "vapid t={signing_input}.{}, k={}",
            b64(&sig.to_bytes()),
            self.public_key()
        ))
    }
}

/// Encrypts `plaintext` for a subscription (its `p256dh` key and `auth`
/// secret), as one `aes128gcm` record (RFC 8291). `ephemeral` and `salt`
/// are fixed only by tests; otherwise random.
pub fn encrypt(
    plaintext: &[u8],
    ua_public: &[u8],
    auth_secret: &[u8],
    ephemeral: Option<SecretKey>,
    salt: Option<[u8; 16]>,
) -> Result<Vec<u8>> {
    anyhow::ensure!(auth_secret.len() == 16, "auth secret must be 16 bytes");
    anyhow::ensure!(plaintext.len() <= 3993, "push payload too large");
    let ua = PublicKey::from_sec1_bytes(ua_public).context("bad p256dh key")?;
    let ua_bytes = ua.to_encoded_point(false);
    let as_secret = match ephemeral {
        Some(k) => k,
        None => random_secret()?,
    };
    let as_public = as_secret.public_key().to_encoded_point(false);
    let salt = match salt {
        Some(s) => s,
        None => {
            let mut s = [0u8; 16];
            getrandom::fill(&mut s).map_err(|e| anyhow!("no randomness: {e}"))?;
            s
        }
    };
    let shared = p256::ecdh::diffie_hellman(as_secret.to_nonzero_scalar(), ua.as_affine());

    // IKM = HKDF(auth_secret, ecdh, "WebPush: info" || 0 || ua || as, 32)
    let mut key_info = b"WebPush: info\0".to_vec();
    key_info.extend_from_slice(ua_bytes.as_bytes());
    key_info.extend_from_slice(as_public.as_bytes());
    let mut ikm = [0u8; 32];
    Hkdf::<Sha256>::new(Some(auth_secret), shared.raw_secret_bytes())
        .expand(&key_info, &mut ikm)
        .map_err(|_| anyhow!("hkdf"))?;
    let prk = Hkdf::<Sha256>::new(Some(&salt), &ikm);
    let mut cek = [0u8; 16];
    let mut nonce = [0u8; 12];
    prk.expand(b"Content-Encoding: aes128gcm\0", &mut cek)
        .map_err(|_| anyhow!("hkdf"))?;
    prk.expand(b"Content-Encoding: nonce\0", &mut nonce)
        .map_err(|_| anyhow!("hkdf"))?;

    // One record: the data, then the last-record delimiter 0x02.
    let mut padded = plaintext.to_vec();
    padded.push(2);
    let ct = Aes128Gcm::new_from_slice(&cek)
        .map_err(|_| anyhow!("aes key"))?
        .encrypt(Nonce::from_slice(&nonce), padded.as_slice())
        .map_err(|_| anyhow!("encryption failed"))?;

    // Header: salt, record size (4096), key id length, key id (as_public).
    let mut out = Vec::with_capacity(86 + ct.len());
    out.extend_from_slice(&salt);
    out.extend_from_slice(&4096u32.to_be_bytes());
    out.push(65);
    out.extend_from_slice(as_public.as_bytes());
    out.extend_from_slice(&ct);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use p256::ecdsa::VerifyingKey;
    use p256::ecdsa::signature::Verifier;

    /// RFC 8291, Appendix A.
    #[test]
    fn rfc8291_example() {
        let as_private = unb64("yfWPiYE-n46HLnH0KqZOF1fJJU3MYrct3AELtAQ-oRw").unwrap();
        let ua_public = unb64(
            "BCVxsr7N_eNgVRqvHtD0zTZsEc6-VV-JvLexhqUzORcxaOzi6-AYWXvTBHm4bjyPjs7Vd8pZGH6SRpkNtoIAiw4",
        )
        .unwrap();
        let salt: [u8; 16] = unb64("DGv6ra1nlYgDCS1FRnbzlw").unwrap().try_into().unwrap();
        let auth = unb64("BTBZMqHH6r4Tts7J_aSIgg").unwrap();
        let body = encrypt(
            b"When I grow up, I want to be a watermelon",
            &ua_public,
            &auth,
            Some(SecretKey::from_slice(&as_private).unwrap()),
            Some(salt),
        )
        .unwrap();
        assert_eq!(
            b64(&body),
            "DGv6ra1nlYgDCS1FRnbzlwAAEABBBP4z9KsN6nGRTbVYI_c7VJSPQTBtkgcy27mlmlMoZIIgDll6e3vCYLocInmYWAmS6TlzAC8wEqKK6PBru3jl7A_yl95bQpu6cVPTpK4Mqgkf1CXztLVBSt2Ks3oZwbuwXPXLWyouBWLVWGNWQexSgSxsj_Qulcy4a-fN"
        );
    }

    #[test]
    fn vapid_header_verifies() {
        let dir = tempfile::tempdir().unwrap();
        let v = Vapid::load_or_create(dir.path()).unwrap();
        // Reloads the same key; the file is private.
        let again = Vapid::load_or_create(dir.path()).unwrap();
        assert_eq!(v.public_key(), again.public_key());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("vapid.key"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
        let h = v
            .authorization(
                "https://fcm.googleapis.com/fcm/send/abc",
                "https://example.com",
                1_000,
            )
            .unwrap();
        let (t, k) = h
            .strip_prefix("vapid t=")
            .unwrap()
            .split_once(", k=")
            .unwrap();
        assert_eq!(k, v.public_key());
        let parts: Vec<&str> = t.split('.').collect();
        let claims: serde_json::Value = serde_json::from_slice(&unb64(parts[1]).unwrap()).unwrap();
        assert_eq!(claims["aud"], "https://fcm.googleapis.com");
        assert_eq!(claims["exp"], 1_000 + 12 * 3600);
        let key = VerifyingKey::from_sec1_bytes(&unb64(k).unwrap()).unwrap();
        let sig = Signature::from_slice(&unb64(parts[2]).unwrap()).unwrap();
        key.verify(format!("{}.{}", parts[0], parts[1]).as_bytes(), &sig)
            .unwrap();
    }
}
