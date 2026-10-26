//! The Webapp's settings and addressing contract (port of
//! `gateway/webapp_listener.py::webapp_settings` and the `_host`/`_public_url`
//! helpers).
//!
//! The `webapp:` block of the gateway home's `config.yaml` decides whether the
//! listener serves the Webapp and on what address. A missing, unreadable or
//! malformed block reads as off, so a typo never exposes the listener; a block
//! that cannot be used carries the reason in [`WebappSettings::reason`].
//!
//! Parsing note (deferral): the Python reader runs a full YAML loader. The Rust
//! port reads the block with a focused parser for the shape the Webapp needs —
//! a top-level `webapp:` mapping with the scalar keys `enabled`, `host`,
//! `port`, `public_url`. Anchors, multiline scalars and flow mappings are not
//! supported; a config that uses them reads as the default (off), never as a
//! partially applied address.

/// `webapp.host` default: the loopback bind the sign-in gate leaves open
/// (`DEFAULT_HOST`).
pub const DEFAULT_HOST: &str = "127.0.0.1";
/// `webapp.port` default (`DEFAULT_PORT`): the documented stable address, so
/// the browser's URL survives gateway restarts.
pub const DEFAULT_PORT: u16 = 9120;
/// Where the browser credential lives under the home (`TOKEN_PATH`).
pub const TOKEN_PATH: &str = "webapp/session-token";
/// The bundle-freshness marker `hermes update` writes when a rebuild failed
/// (`hermes_cli/webapp.py::STALE_MARKER`).
pub const STALE_MARKER: &str = ".hermes-stale";

/// How every missing-bundle reason starts (`NOT_BUILT`).
pub const NOT_BUILT: &str = "The Webapp is not built";
/// Why a bundle `hermes update` could not rebuild is not served (`NOT_REBUILT`).
pub const NOT_REBUILT: &str = concat!(
    "The Webapp is not built for this version of Hermes: hermes update could not rebuild it. ",
    "Run hermes webapp to rebuild it."
);

/// The hosts the listener's sign-in gate leaves open as local-only
/// (`web_server_boundary.should_require_auth` knows exactly these three).
pub const LOCAL_ONLY_HOSTS: [&str; 3] = ["127.0.0.1", "::1", "localhost"];
/// The wildcard binds (`_is_wildcard`); they always engage the sign-in gate.
pub const WILDCARD_HOSTS: [&str; 2] = ["0.0.0.0", "::"];

/// `webapp:` from *home*'s config, read the way
/// `gateway/webapp_listener.py::WebappSettings` reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WebappSettings {
    pub enabled: bool,
    pub host: String,
    pub port: u16,
    /// `webapp.public_url`: the address a reverse proxy or tunnel serves the
    /// Webapp at, `""` for none.
    pub public_url: String,
    /// Why a `webapp:` block that asks for the Webapp keeps it off, else `None`.
    pub reason: Option<String>,
    /// Whether `webapp.port` was configured (the launcher keeps a configured
    /// port and only searches when the home has none).
    pub port_configured: bool,
}

impl Default for WebappSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            host: DEFAULT_HOST.into(),
            port: DEFAULT_PORT,
            public_url: String::new(),
            reason: None,
            port_configured: false,
        }
    }
}

impl WebappSettings {
    /// The listener's bind address with the Webapp on, else a private loopback
    /// listener on an ephemeral port (the `bind` property).
    pub fn bind(&self) -> (String, u16) {
        if self.enabled {
            (self.host.clone(), self.port)
        } else {
            (DEFAULT_HOST.into(), 0)
        }
    }
}

/// The `webapp:` block as written: raw scalar strings, `None` = key absent.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
struct RawBlock {
    enabled: Option<String>,
    host: Option<String>,
    port: Option<String>,
    public_url: Option<String>,
}

/// `webapp:` from *home*'s `config.yaml`. See the module docs for the parser's
/// scope. Unreadable → default (off); malformed values keep the valid host,
/// port and public URL (so status shows the address) and carry `reason`.
pub fn webapp_settings(home: &std::path::Path) -> WebappSettings {
    let path = home.join("config.yaml");
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let block = read_block(&text);
    // Canonical host, `None` when not one of the five the block may name.
    let host: Option<String> = block
        .host
        .as_deref()
        .map(|h| canonical_host(&bare_host(h)))
        .filter(|h| is_known_host(h));
    let port: Option<u16> = block.port.as_deref().and_then(|p| p.parse::<u16>().ok());
    let public_url: Option<String> = match block.public_url.as_deref() {
        // Absent and empty both read as "" (`_public_url(None)`).
        None | Some("") => Some(String::new()),
        Some(raw) => public_url(raw),
    };
    let configured = WebappSettings {
        host: host.clone().unwrap_or_else(|| DEFAULT_HOST.into()),
        port: port.unwrap_or(DEFAULT_PORT),
        port_configured: port.is_some(),
        public_url: public_url.clone().unwrap_or_default(),
        ..Default::default()
    };
    let off = |reason: String| WebappSettings { reason: Some(reason), ..configured.clone() };
    match block.enabled.as_deref() {
        None | Some("false") => configured,
        Some("true") => {
            if block.host.is_some() && host.is_none() {
                return off(format!(
                    "webapp.host must be 127.0.0.1, ::1, localhost, 0.0.0.0 or :: in {}",
                    path.display()
                ));
            }
            if block.port.is_some() && port.is_none() {
                return off(format!(
                    "webapp.port must be a port number (1-65535) in {}",
                    path.display()
                ));
            }
            if block.public_url.is_some() && public_url.is_none() {
                return off(format!(
                    "webapp.public_url must be an http:// or https:// address with no credentials, \
                     query or fragment in {}",
                    path.display()
                ));
            }
            WebappSettings { enabled: true, ..configured }
        }
        Some(other) => off(format!(
            "webapp.enabled must be true or false in {path:?} (got {other:?})"
        )),
    }
}

/// Whether *host* is one of the five hosts the block may name.
fn is_known_host(host: &str) -> bool {
    LOCAL_ONLY_HOSTS.contains(&host) || WILDCARD_HOSTS.contains(&host)
}

/// *host* without surrounding space or the brackets a URL puts around an IPv6
/// address (`_bare_host`): `[::1]` is `::1`; brackets around anything else stay
/// and fail the host check.
pub fn bare_host(host: &str) -> String {
    let host = host.trim();
    if host.starts_with('[') && host.ends_with(']') && host.len() > 2 {
        let inner = &host[1..host.len() - 1];
        if inner.contains(':') && !inner.contains('[') {
            return inner.to_string();
        }
    }
    host.to_string()
}

/// *host* spelled as the sign-in gate compares it (`_canonical_host`): an IP
/// address in its short form (`0:0:0:0:0:0:0:1` is `::1`), a name lower-cased.
/// The Rust port knows the one long-form IPv6 spelling the config contract
/// needs (`0:0:0:0:0:0:0:1` → `::1`) and otherwise lower-cases.
pub fn canonical_host(host: &str) -> String {
    let lowered = host.to_ascii_lowercase();
    if lowered == "0:0:0:0:0:0:0:1" {
        "::1".into()
    } else {
        lowered
    }
}

/// Whether the sign-in gate leaves a bind to *host* open as local-only
/// (`_local_only`): exactly `127.0.0.1`, `::1` and `localhost`; any other
/// address, another loopback one included, engages sign-in like a public bind.
pub fn local_only(host: &str) -> bool {
    LOCAL_ONLY_HOSTS.contains(&host)
}

/// Whether *host* is a wildcard bind (`_is_wildcard`).
pub fn is_wildcard(host: &str) -> bool {
    WILDCARD_HOSTS.contains(&host)
}

/// `webapp.public_url` without a trailing slash, `""` when unset, `None` when
/// malformed (`_public_url`): not an http(s) address, or one carrying
/// credentials, a query or a fragment.
pub fn public_url(value: &str) -> Option<String> {
    if value.is_empty() {
        return Some(String::new());
    }
    if value
        .chars()
        .any(|c| "\"'<> \t\r\n?#@\\".contains(c))
    {
        return None;
    }
    let url = value.trim_end_matches('/');
    let (scheme, rest) = url.split_once("://")?;
    if !matches!(scheme, "http" | "https") {
        return None;
    }
    let authority = rest.split(['/', '?']).next().unwrap_or("");
    // user:pass@host is a credential the listener refuses.
    if authority.contains('@') {
        return None;
    }
    let host_part = authority.rsplit_once(':').map_or(authority, |(h, _)| h);
    let host = host_part.trim_matches(|c| c == '[' || c == ']');
    if host.is_empty() {
        return None;
    }
    if let Some(port) = authority.rsplit_once(':').map(|(_, p)| p) {
        if !port.is_empty() && port.parse::<u16>().ok()? == 0 {
            return None;
        }
    }
    Some(url.to_string())
}

/// The loopback address native clients dial for a listener bound to
/// *bind_host* (`dial_host`).
pub fn dial_host(bind_host: &str) -> String {
    if bind_host == "localhost" || bind_host == "127.0.0.1" || bind_host == "::1" {
        return bind_host.to_string();
    }
    if bind_host == "::" { "::1".into() } else { "127.0.0.1".into() }
}

/// The `api_origin` native clients get (`dial_origin`).
pub fn dial_origin(bind_host: &str, port: u16) -> String {
    let host = dial_host(bind_host);
    if host.contains(':') {
        format!("http://[{host}]:{port}")
    } else {
        format!("http://{host}:{port}")
    }
}

/// Why the Webapp cannot be served from *dist*, or `None` once it is built
/// (`bundle_missing_reason`). The listener serves a bundle the moment it
/// appears, so building it needs no gateway restart.
pub fn bundle_missing_reason(dist: &std::path::Path) -> Option<String> {
    if dist.join("index.html").is_file() {
        return bundle_stale_reason(dist);
    }
    Some(format!(
        "{NOT_BUILT} ({} is missing). Run hermes webapp to build it.",
        dist.join("index.html").display()
    ))
}

/// `NOT_REBUILT` while the bundle in *dist* is marked out of date
/// (`bundle_stale_reason`).
pub fn bundle_stale_reason(dist: &std::path::Path) -> Option<String> {
    if dist.join(STALE_MARKER).is_file() {
        Some(NOT_REBUILT.to_string())
    } else {
        None
    }
}

// ── the focused config.yaml reader ──────────────────────────────────────────

/// Read the top-level `webapp:` mapping's scalar keys from *text*.
fn read_block(text: &str) -> RawBlock {
    let mut block = RawBlock::default();
    let mut in_block = false;
    for line in text.lines() {
        let indent = line.len() - line.trim_start().len();
        let trimmed = line.trim_end();
        if trimmed.is_empty() || trimmed.trim_start().starts_with('#') {
            continue;
        }
        if indent == 0 {
            if in_block {
                break; // the next top-level key ends the block
            }
            if let Some((key, rest)) = trimmed.split_once(':') {
                if key.trim() == "webapp" && rest.trim().is_empty() {
                    in_block = true;
                }
            }
            continue;
        }
        if !in_block {
            continue;
        }
        if let Some((key, value)) = trimmed.split_once(':') {
            let key = key.trim();
            let value = clean_value(value);
            match key {
                "enabled" => block.enabled = Some(value),
                "host" => block.host = Some(value),
                "port" => block.port = Some(value),
                "public_url" => block.public_url = Some(value),
                _ => {}
            }
        }
    }
    block
}

/// Strip a trailing `# comment`, surrounding whitespace and one pair of quotes.
fn clean_value(value: &str) -> String {
    let mut value = value.trim();
    // A `#` after a space starts a comment; inside an unquoted URL it does not,
    // and the URL validator refuses a bare `#` anyway.
    if let Some(pos) = value.find(" #") {
        value = value[..pos].trim();
    }
    if (value.starts_with('"') && value.ends_with('"') && value.len() > 1)
        || (value.starts_with('\'') && value.ends_with('\'') && value.len() > 1)
    {
        value = &value[1..value.len() - 1];
    }
    value.to_string()
}
