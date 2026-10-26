//! The Webapp's browser credential (port of
//! `gateway/webapp_listener.py::browser_session_token`) and the sign-in
//! binding primitives (port of `hermes_cli/webapp_sign_in.py::binding` and
//! `::_gateway`).
//!
//! The browser's credential is the dashboard session token, kept user-only at
//! `<home>/webapp/session-token` so open tabs keep working across a gateway
//! restart; rotating it (sign-out) signs every open tab out. Created on first
//! use; a stored token that is unreadable, not text or malformed is replaced.
//! The token shape is `secrets.token_urlsafe(32)`: 43 characters of the
//! base64url alphabet — the launch page refuses any other shape.

use std::sync::atomic::{AtomicU64, Ordering};

/// Where the gateway home keeps the digests of the provider tokens its
/// listener gave native apps (`webapp_sign_in.RECORD_PATH`).
pub const NATIVE_TOKEN_RECORD_PATH: &str = "webapp/native-sign-in-tokens.json";
/// How long a refresh token stays recorded (`REFRESH_TOKEN_LIFETIME`, the
/// bundled password provider's 30 days).
pub const REFRESH_TOKEN_LIFETIME_SECS: u64 = 30 * 24 * 60 * 60;

/// The token shape the launch page and the window handoff enforce
/// (`_TOKEN_FORMAT`: `[A-Za-z0-9_-]{43}`).
pub fn valid_token(token: &str) -> bool {
    token.len() == 43
        && token
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// Splitmix64-based generator: the Rust port has no `rand` dependency, so
/// tokens come from a splitmix64 stream seeded by the clock and a process
/// counter — the same shape budget as the desktop `new_id` helper.
fn next_word() -> u64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    fn mix(mut x: u64) -> u64 {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    let pid = std::process::id() as u64;
    mix(nanos ^ mix(pid ^ mix(COUNTER.fetch_add(1, Ordering::Relaxed))))
}

/// A fresh 43-character urlsafe-shaped token.
pub fn generate_token() -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789_-";
    let mut out = String::with_capacity(43);
    let mut word = 0u64;
    for i in 0..43 {
        if i % 10 == 0 {
            word = next_word();
        }
        out.push(ALPHABET[(word & 0x3f) as usize] as char);
        word >>= 6;
    }
    out
}

/// The credential store behind `browser_session_token`.
#[derive(Debug)]
pub struct BrowserCredential {
    home: std::path::PathBuf,
    token: String,
    /// The generation counter `browser_credential` tracks: bumped at rotate,
    /// stamped on handoff tickets and sign-in bindings so a ticket or binding
    /// minted before a sign-out opens nothing after it.
    generation: AtomicU64,
}

#[derive(Debug)]
pub struct CredentialError(pub String);

impl std::fmt::Display for CredentialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for CredentialError {}

impl BrowserCredential {
    /// Load the stored token, creating a fresh one on first use or when the
    /// stored one is malformed (`browser_session_token(home)`).
    pub fn load(home: &std::path::Path) -> Result<Self, CredentialError> {
        let mut store = Self {
            home: home.into(),
            token: String::new(),
            generation: AtomicU64::new(1),
        };
        store.reload(false)?;
        Ok(store)
    }

    fn token_path(&self) -> std::path::PathBuf {
        self.home.join(super::settings::TOKEN_PATH)
    }

    fn reload(&mut self, rotate: bool) -> Result<(), CredentialError> {
        if rotate {
            self.generation.fetch_add(1, Ordering::Relaxed);
        } else if let Ok(raw) = std::fs::read_to_string(self.token_path()) {
            // utf-8-sig: a BOM the file may carry is stripped with the rest of
            // the surrounding whitespace, like the Python `read_text(...).strip()`.
            let token = raw.trim_matches(|c: char| c.is_whitespace() || c == '\u{feff}');
            if valid_token(token) {
                self.token = token.to_string();
                return Ok(());
            }
        }
        self.token = generate_token();
        self.write()
    }

    fn write(&self) -> Result<(), CredentialError> {
        let path = self.token_path();
        let parent = path
            .parent()
            .ok_or_else(|| CredentialError(format!("{} has no parent folder", path.display())))?;
        std::fs::create_dir_all(parent).map_err(|e| {
            CredentialError(format!(
                "could not store the Webapp's browser credential in {} ({e})",
                parent.display()
            ))
        })?;
        // Temp + rename: a concurrent reader never sees a half-written token.
        let tmp = parent.join("session-token.tmp");
        std::fs::write(&tmp, format!("{}\n", self.token)).map_err(|e| {
            CredentialError(format!(
                "could not store the Webapp's browser credential in {} ({e})",
                path.display()
            ))
        })?;
        std::fs::rename(&tmp, &path).map_err(|e| {
            CredentialError(format!(
                "could not store the Webapp's browser credential in {} ({e})",
                path.display()
            ))
        })?;
        Ok(())
    }

    /// The dashboard session token a Webapp browser holds.
    pub fn token(&self) -> &str {
        &self.token
    }

    /// The generation the current token was minted at.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Relaxed)
    }

    /// Whether *generation* is the credential's current one
    /// (`browser_credential.is_current`).
    pub fn is_current(&self, generation: u64) -> bool {
        self.generation() == generation
    }

    /// The token a request presented, or `None`: the `Authorization: Bearer`
    /// credential. (The Rust listener's browsers authenticate with the session
    /// token itself, where the Python surface also accepts session cookies.)
    pub fn presented<'a>(&self, authorization: Option<&'a str>) -> Option<&'a str> {
        let presented = authorization?.strip_prefix("Bearer ")?.trim();
        (presented == self.token).then_some(presented)
    }

    /// Replace the token, which signs every open tab out (`rotate=True`):
    /// the file changes and every grant made under the old credential ends.
    pub fn rotate(&mut self) -> Result<String, CredentialError> {
        self.reload(true)?;
        Ok(self.token.clone())
    }
}

// ── the sign-in binding (webapp_sign_in.py) ─────────────────────────────────

const SIGN_IN_MESSAGE: &[u8] = b"webapp-sign-in";

/// The `hermes_webapp_signin_<gateway>` cookie value for browser session token
/// *token* (`webapp_sign_in.binding`): the hex HMAC-SHA256 of
/// `"webapp-sign-in"` keyed with the token. Python's `hmac.new` is reproduced
/// with the in-crate SHA-256 below (no crypto dependency).
pub fn binding(token: &str) -> String {
    hex(&hmac_sha256(token.as_bytes(), SIGN_IN_MESSAGE))
}

/// The suffix this gateway's Webapp gives every sign-in cookie it sets
/// (`_gateway`): a digest of its home, the same across restarts and different
/// for every gateway on one machine; `""` without a home.
pub fn cookie_gateway(home: Option<&std::path::Path>) -> String {
    match home {
        None => String::new(),
        Some(home) => hex(&sha256(home.display().to_string().as_bytes()))[..16].to_string(),
    }
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

// ── SHA-256 (FIPS 180-4) + HMAC (RFC 2104), dependency-free ─────────────────

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5,
    0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174,
    0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da,
    0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967,
    0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85,
    0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070,
    0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
    0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7, 0xc67178f2,
];

/// One-shot SHA-256.
pub fn sha256(data: &[u8]) -> [u8; 32] {
    let mut state: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let bitlen = (data.len() as u64).wrapping_mul(8);
    let mut padded = data.to_vec();
    padded.push(0x80);
    while padded.len() % 64 != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bitlen.to_be_bytes());
    for block in padded.chunks_exact(64) {
        let mut w = [0u32; 64];
        for (i, word) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes(word.try_into().unwrap());
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut h] = state;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = h
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            h = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        state[0] = state[0].wrapping_add(a);
        state[1] = state[1].wrapping_add(b);
        state[2] = state[2].wrapping_add(c);
        state[3] = state[3].wrapping_add(d);
        state[4] = state[4].wrapping_add(e);
        state[5] = state[5].wrapping_add(f);
        state[6] = state[6].wrapping_add(g);
        state[7] = state[7].wrapping_add(h);
    }
    let mut out = [0u8; 32];
    for (i, word) in state.iter().enumerate() {
        out[i * 4..][..4].copy_from_slice(&word.to_be_bytes());
    }
    out
}

/// HMAC-SHA256 over *key* and *message*.
pub fn hmac_sha256(key: &[u8], message: &[u8]) -> [u8; 32] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..32].copy_from_slice(&sha256(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    let mut inner = ipad;
    inner.extend_from_slice(message);
    let mut outer = opad;
    outer.extend_from_slice(&sha256(&inner));
    sha256(&outer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_match_the_launch_page_shape() {
        for _ in 0..8 {
            let t = generate_token();
            assert_eq!(t.len(), 43);
            assert!(valid_token(&t));
        }
        assert!(valid_token(&"A".repeat(43)));
        assert!(!valid_token(&"A".repeat(42)));
        assert!(!valid_token(&"A.".repeat(22)));
    }

    #[test]
    fn sha256_matches_the_fips_vectors() {
        // FIPS 180-4 appendix vectors.
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
    }

    #[test]
    fn hmac_matches_rfc_4231_case_1_and_2() {
        // RFC 4231 test case 1.
        let key = [0x0b; 20];
        let mac = hex(&hmac_sha256(&key, b"Hi There"));
        assert_eq!(
            mac,
            "b0344c61d8db38535ca8afceaf0bf12b881dc200c9833da726e9376c2e32cff7"
        );
        // Test case 2.
        let key: [u8; 20] = {
            let mut k = [0u8; 20];
            k[..16].copy_from_slice(b"JefeJefeJefeJefe");
            k
        };
        assert_eq!(
            hex(&hmac_sha256(&key, b"what do ya want for nothing?")),
            "2ccd85dfc8d18cb5d84fef4b198554699fece6e8692c9147b0da983f5b7bd413"
        );
    }

    #[test]
    fn credential_loads_rotates_and_replaces_malformed() {
        let dir = std::env::temp_dir().join(format!("hermes-webapp-cred-{}", std::process::id()));
        let home = dir.join("case-a");
        std::fs::create_dir_all(&home).unwrap();
        let mut cred = BrowserCredential::load(&home).unwrap();
        let first = cred.token().to_string();
        // A second load of the same home reuses the stored token.
        let again = BrowserCredential::load(&home).unwrap();
        assert_eq!(again.token(), first);
        // Rotate replaces the token and bumps the generation.
        let gen = cred.generation();
        let second = cred.rotate().unwrap();
        assert_ne!(second, first);
        assert_eq!(cred.generation(), gen + 1);
        assert!(cred.is_current(gen + 1));
        assert!(!cred.is_current(gen));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn malformed_stored_token_is_replaced() {
        let dir = std::env::temp_dir().join(format!("hermes-webapp-cred-bad-{}", std::process::id()));
        let home = dir.join("case-b");
        std::fs::create_dir_all(home.join("webapp")).unwrap();
        std::fs::write(home.join("webapp/session-token"), "short\n").unwrap();
        let cred = BrowserCredential::load(&home).unwrap();
        assert!(valid_token(cred.token()));
        assert_ne!(cred.token(), "short");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn presented_bearer_must_match() {
        let dir = std::env::temp_dir().join(format!("hermes-webapp-cred-bearer-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let cred = BrowserCredential::load(&dir).unwrap();
        let token = cred.token().to_string();
        assert_eq!(cred.presented(Some(&format!("Bearer {token}"))), Some(token.as_str()));
        assert_eq!(cred.presented(Some("Bearer other")), None);
        assert_eq!(cred.presented(None), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn binding_is_stable_and_cookie_suffix_differs_per_home() {
        let b = binding("tok");
        assert_eq!(b.len(), 64);
        assert_eq!(b, binding("tok"));
        assert_ne!(b, binding("tok2"));
        assert_eq!(cookie_gateway(None), "");
        let a = cookie_gateway(Some(std::path::Path::new("/homes/a")));
        let c = cookie_gateway(Some(std::path::Path::new("/homes/c")));
        assert_eq!(a.len(), 16);
        assert_ne!(a, c);
    }
}
