//! Shared helpers for the web tool modules — the `plugins/web/_common.py`
//! result shapes (`page_error`, `title_hit`) plus the cache-path plumbing of
//! `web_tools_truncate._store_full_text` / `web_result_cache._host_slug`.

use serde_json::json;

/// Process-global blocking HTTP client (never dropped, so its internal tokio
/// runtime can't panic on drop inside an async context; pools are reused —
/// the same caching the Python plugins do on their `<vendor>_client` slots).
/// The client is built on a dedicated OS thread: building `reqwest::blocking`
/// on a tokio runtime thread drops a runtime in an async context. Per-request
/// timeouts are set with `RequestBuilder::timeout`.
pub fn http_client() -> &'static reqwest::blocking::Client {
    static CLIENT: std::sync::OnceLock<reqwest::blocking::Client> = std::sync::OnceLock::new();
    CLIENT.get_or_init(|| {
        std::thread::spawn(|| {
            reqwest::blocking::Client::builder()
                .user_agent("hermes-agent/1.0 (+web tools)")
                .build()
                .expect("build blocking HTTP client")
        })
        .join()
        .expect("blocking HTTP client builder thread")
    })
}

/// `plugins/web/_common.py::page_error` — failed extract entry.
pub fn page_error(url: &str, error: &str) -> serde_json::Value {
    json!({"url": url, "title": "", "content": "", "error": error})
}

/// `web_tools_extract._result_entry` — `{url, title, content, error}`.
pub fn result_entry(url: &str, error: &str) -> serde_json::Value {
    json!({"url": url, "title": "", "content": "", "error": error})
}

/// `web_result_cache._host_slug`: lowercase host, non-alnum runs → `-`.
pub fn host_slug(url: &str) -> String {
    let host = crate::web_extract::url_host(url).unwrap_or_default();
    let mut slug = String::with_capacity(host.len());
    let mut prev_dash = false;
    for c in host.chars() {
        if c.is_ascii_alphanumeric() || c == '.' {
            slug.push(c.to_ascii_lowercase());
            prev_dash = false;
        } else if !prev_dash {
            slug.push('-');
            prev_dash = true;
        }
    }
    slug.trim_matches('-').to_string()
}

/// `hermes_constants.get_hermes_dir("cache/web")`: the Hermes home's
/// `cache/web` directory (`HERMES_DIR` override, else `~/.hermes`).
pub fn hermes_cache_web_dir() -> Option<std::path::PathBuf> {
    let base = match std::env::var("HERMES_DIR") {
        Ok(dir) if !dir.is_empty() => std::path::PathBuf::from(dir),
        _ => {
            let home = crate::read_file::expand_tilde("~/.hermes");
            // expand_tilde only rewrites a leading `~`; join for the suffix.
            if home.to_string_lossy().starts_with('~') {
                return None;
            }
            home
        }
    };
    Some(base.join("cache").join("web"))
}

/// `web_tools_truncate.convert_base64_images_to_links`: replace inline base64
/// image blobs (token bombs) with `[IMAGE: alt]` placeholders. Real http(s)
/// markdown image links are left untouched.
pub fn base64_images_to_links(text: &str) -> String {
    use regex::Regex;
    // Markdown images: ![alt](data:image/...;base64,payload)
    let md =
        Regex::new(r"(?s)!\[(?P<alt>[^\]]*)\]\(\s*data:image/[^;]+;base64,[A-Za-z0-9+/=\s]+\)")
            .unwrap();
    let out = md.replace_all(text, |caps: &regex::Captures| {
        let alt = caps.get(1).map(|m| m.as_str().trim()).unwrap_or("");
        if alt.is_empty() {
            "[IMAGE]".to_string()
        } else {
            format!("[IMAGE: {alt}]")
        }
    });
    // Parenthesised blobs.
    let paren = Regex::new(r"\(\s*data:image/[^;]+;base64,[A-Za-z0-9+/=\s]+\)").unwrap();
    let out = paren.replace_all(&out, "[IMAGE]");
    // Bare payloads.
    let bare = Regex::new(r"data:image/[^;]+;base64,[A-Za-z0-9+/=]+").unwrap();
    bare.replace_all(&out, "[IMAGE]").into_owned()
}

/// Deterministic 10-hex-digit URL digest for the cached full-text filename —
/// the `sha256(url)[:10]` of `_store_full_text`, implemented locally so the
/// tools crate needs no external hash crate.
pub fn sha256_hex10(bytes: &[u8]) -> String {
    let digest = sha256(bytes);
    digest[..5].iter().map(|b| format!("{b:02x}")).collect()
}

/// Minimal FIPS-180 SHA-256 (public-domain algorithm, standard constants).
pub fn sha256(bytes: &[u8]) -> [u8; 32] {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];
    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];
    let mut msg = bytes.to_vec();
    let bit_len = (bytes.len() as u64) * 8;
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());
    for block in msg.chunks_exact(64) {
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
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }
    h.iter()
        .flat_map(|w| w.to_be_bytes())
        .collect::<Vec<u8>>()
        .try_into()
        .unwrap()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vectors() {
        // FIPS-180 test vectors.
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

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    #[test]
    fn hex10_is_ten_hex_digits() {
        let out = sha256_hex10(b"https://example.com");
        assert_eq!(out.len(), 10);
        assert!(out.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn slugs() {
        assert_eq!(host_slug("https://Example.com/path"), "example.com");
        assert_eq!(host_slug("http://a-b_c.example:8080/x"), "a-b-c.example");
    }

    #[test]
    fn base64_blobs_become_placeholders() {
        let blob = "data:image/png;base64,iVBORw0KGgoAAAANSUhEUgAAAAUA";
        assert_eq!(
            base64_images_to_links(&format!("![a chart]({blob})")),
            "[IMAGE: a chart]"
        );
        assert_eq!(
            base64_images_to_links(&format!("see ({blob})")),
            "see [IMAGE]"
        );
        assert_eq!(base64_images_to_links(blob), "[IMAGE]");
        // Real image links are untouched.
        let md = "![pic](https://e.com/i.png)";
        assert_eq!(base64_images_to_links(md), md);
    }
}
