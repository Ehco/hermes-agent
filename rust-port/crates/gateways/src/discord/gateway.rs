//! Discord gateway websocket — port of the connect/heartbeat/identify/dispatch
//! loop of `plugins/platforms/discord/adapter.py`.
//!
//! The Python adapter delegates the gateway to the `discord.py` library; the
//! Rust port is self-contained: a minimal RFC6455 client (masking, text/binary,
//! close, ping/pong) over a generic `AsyncRead + AsyncWrite` stream, with the
//! Discord gateway protocol on top:
//!
//! - op 10 HELLO → `d.heartbeat_interval` + IDENTIFY (op 2),
//! - heartbeat (op 1) every `heartbeat_interval` carrying the last sequence,
//! - op 11 HEARTBEAT ACK / op 1 server heartbeat → op 1 reply,
//! - op 0 DISPATCH → sequence tracking (`READY` captures `session_id`),
//!   `MESSAGE_CREATE` becomes a shared-contract [`InboundMessage`],
//! - op 7 RECONNECT / op 9 INVALID SESSION → resume (op 6) / re-identify,
//! - close frame or stream error → returns, caller reconnects.
//!
//! TLS deferral: `gateway.discord.gg` is `wss://`. The workspace has no TLS
//! websocket dependency, so [`connect`] speaks the RFC6455 handshake over a
//! plain tokio `TcpStream`; the stream is generic, so a TLS implementation
//! drops in without touching the protocol code (see port notes).

use crate::contract::{BoxFut, InboundMessage, Platform};
use anyhow::{anyhow, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// Gateway version + JSON encoding (the URL the Python bot connects to).
pub const GATEWAY_URL: &str = "wss://gateway.discord.gg/?v=10&encoding=json";

/// Default gateway intents: GUILD_MESSAGES | DIRECT_MESSAGES | MESSAGE_CONTENT.
/// (The Python resolves privileged intents from config; the message-content
/// privileged intent is required to see non-mention message text.)
pub const DEFAULT_INTENTS: u64 = 512 | 256 | 32768;

pub const OP_CONTINUATION: u8 = 0;
pub const OP_TEXT: u8 = 1;
pub const OP_BINARY: u8 = 2;
pub const OP_CLOSE: u8 = 8;
pub const OP_PING: u8 = 9;
pub const OP_PONG: u8 = 10;

/// Discord gateway opcodes (the `op` field of every gateway payload).
pub const GW_HEARTBEAT: u64 = 1;
pub const GW_IDENTIFY: u64 = 2;
pub const GW_RESUME: u64 = 6;
pub const GW_RECONNECT: u64 = 7;
pub const GW_INVALID_SESSION: u64 = 9;
pub const GW_HELLO: u64 = 10;
pub const GW_HEARTBEAT_ACK: u64 = 11;

// ---------------------------------------------------------------------------
// RFC6455 framing
// ---------------------------------------------------------------------------

/// One decoded websocket frame (fragmentation is reassembled by [`read_frame`]).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub fin: bool,
    pub opcode: u8,
    pub payload: Vec<u8>,
}

/// XOR mask for client→server frames (RFC6455 5.3).
fn apply_mask(payload: &mut [u8], key: [u8; 4]) {
    for (i, b) in payload.iter_mut().enumerate() {
        *b ^= key[i % 4];
    }
}

/// Randomish masking key: nanos xorshift (no `rand` dependency; masking is a
/// transport obfuscation, not a security boundary).
fn mask_key() -> [u8; 4] {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0x9E3779B97F4A7C15);
    let mut x = nanos | 1;
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    (x as u32).to_be_bytes()
}

/// Encode one websocket frame. Client frames MUST be masked.
pub fn encode_frame(mask: bool, opcode: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode); // FIN + opcode (outgoing frames are never fragmented)
    let len = payload.len();
    let mask_bit = if mask { 0x80u8 } else { 0u8 }; // RFC6455 5.3: bit0 of byte 2
    let key = if mask { Some(mask_key()) } else { None };
    match len {
        0..=125 => out.push(mask_bit | len as u8),
        126..=65535 => {
            out.push(mask_bit | 126);
            out.extend_from_slice(&(len as u16).to_be_bytes());
        }
        _ => {
            out.push(mask_bit | 127);
            out.extend_from_slice(&(len as u64).to_be_bytes());
        }
    }
    match key {
        Some(key) => {
            out.extend_from_slice(&key);
            let mut masked = payload.to_vec();
            apply_mask(&mut masked, key);
            out.extend_from_slice(&masked);
        }
        None => out.extend_from_slice(payload),
    }
    out
}

/// Read one (reassembled) frame. Blocks until a full frame arrives.
pub async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Frame> {
    let mut head = [0u8; 2];
    r.read_exact(&mut head).await?;
    let fin = head[0] & 0x80 != 0;
    let opcode = head[0] & 0x0F;
    let masked = head[1] & 0x80 != 0;
    let mut len = (head[1] & 0x7F) as usize;
    if len == 126 {
        let mut ext = [0u8; 2];
        r.read_exact(&mut ext).await?;
        len = u16::from_be_bytes(ext) as usize;
    } else if len == 127 {
        let mut ext = [0u8; 8];
        r.read_exact(&mut ext).await?;
        len = u64::from_be_bytes(ext) as usize;
    }
    let mut key = [0u8; 4];
    if masked {
        r.read_exact(&mut key).await?;
    }
    let mut payload = vec![0u8; len];
    r.read_exact(&mut payload).await?;
    if masked {
        apply_mask(&mut payload, key);
    }
    // Reassemble continuation frames (server→client fragmentation is rare but legal).
    let mut fin = fin;
    while !fin {
        let mut next_head = [0u8; 2];
        r.read_exact(&mut next_head).await?;
        fin = next_head[0] & 0x80 != 0;
        let n = (next_head[1] & 0x7F) as usize;
        let mut tail = vec![0u8; n];
        r.read_exact(&mut tail).await?;
        payload.extend_from_slice(&tail);
    }
    Ok(Frame { fin: true, opcode, payload })
}

/// RFC4648 base64 (for the handshake key).
fn base64_key(bytes: &[u8; 16]) -> String {
    const ALPHABET: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = ((b[0] as u32) << 16) | ((b[1] as u32) << 8) | (b[2] as u32);
        let chars = [
            (n >> 18) as usize & 63,
            (n >> 12) as usize & 63,
            (n >> 6) as usize & 63,
            n as usize & 63,
        ];
        let kept = match chunk.len() {
            3 => 4,
            2 => 3,
            _ => 2,
        };
        for c in chars.iter().take(kept) {
            out.push(ALPHABET[*c] as char);
        }
        for _ in kept..4 {
            out.push('=');
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Gateway protocol state machine (pure, unit-testable)
// ---------------------------------------------------------------------------

/// Sequence/session/heartbeat bookkeeping across the connection.
#[derive(Debug, Default)]
pub struct GatewayState {
    /// Highest dispatch sequence seen (`s` on op 0).
    pub seq: Option<u64>,
    /// Session id captured from the READY dispatch.
    pub session_id: Option<String>,
    /// Heartbeat interval from HELLO (ms).
    pub heartbeat_interval_ms: u64,
    /// True until a heartbeat goes out unanswered (missing ACK = zombie link).
    pub heartbeat_acked: bool,
    /// Token (kept for RESUME payloads).
    pub token: String,
    /// Intent bitfield for IDENTIFY.
    pub intents: u64,
}

impl GatewayState {
    pub fn new(token: &str, intents: u64) -> Self {
        GatewayState {
            token: token.to_string(),
            intents,
            heartbeat_acked: true,
            ..Default::default()
        }
    }
}

/// What the protocol layer asks the connection loop to do.
#[derive(Debug, PartialEq, Eq)]
pub enum GwAction {
    /// Send IDENTIFY (op 2) with the state's token/intents.
    Identify,
    /// Send HEARTBEAT (op 1) with the current sequence.
    Heartbeat,
    /// Send RESUME (op 6) with token/session/seq.
    Resume,
    /// A MESSAGE_CREATE dispatch normalized into the shared contract.
    Inbound(InboundMessage),
    /// Peer asked to reconnect (op 7) or the session went invalid (op 9).
    Reconnect,
    /// Nothing to do.
    None,
}

/// Process one inbound gateway JSON payload; updates `state` and returns the
/// action. Pure apart from the state update — this is the unit-testable core
/// of the dispatch/heartbeat loop.
pub fn handle_payload(state: &mut GatewayState, payload: &Value) -> GwAction {
    let op = payload.get("op").and_then(Value::as_u64).unwrap_or(u64::MAX);
    match op {
        GW_HELLO => {
            state.heartbeat_interval_ms = payload
                .pointer("/d/heartbeat_interval")
                .and_then(Value::as_u64)
                .unwrap_or(41_250);
            state.heartbeat_acked = true;
            if state.session_id.is_some() {
                GwAction::Resume
            } else {
                GwAction::Identify
            }
        }
        GW_HEARTBEAT_ACK => {
            state.heartbeat_acked = true;
            GwAction::None
        }
        GW_HEARTBEAT => {
            // Server-initiated heartbeat: echo ours back immediately.
            GwAction::Heartbeat
        }
        0 => {
            // DISPATCH
            if let Some(s) = payload.get("s").and_then(Value::as_u64) {
                state.seq = Some(s);
            }
            let t = payload.get("t").and_then(Value::as_str).unwrap_or("");
            let d = payload.get("d").cloned().unwrap_or(Value::Null);
            match t {
                "READY" => {
                    state.session_id = d.get("session_id").and_then(Value::as_str).map(String::from);
                    GwAction::None
                }
                "RESUMED" => GwAction::None,
                "MESSAGE_CREATE" => match normalize_message_create(&d) {
                    Some(msg) => GwAction::Inbound(msg),
                    None => GwAction::None,
                },
                _ => GwAction::None,
            }
        }
        GW_RECONNECT => GwAction::Reconnect,
        GW_INVALID_SESSION => {
            // d is a bool: true → resume, false → drop the session and re-identify.
            if payload.get("d").and_then(Value::as_bool).unwrap_or(false) {
                GwAction::Resume
            } else {
                state.session_id = None;
                GwAction::Identify
            }
        }
        _ => GwAction::None,
    }
}

/// The IDENTIFY payload (`{"op":2,"d":{...}}`).
pub fn identify_payload(state: &GatewayState) -> Value {
    json!({
        "op": GW_IDENTIFY,
        "d": {
            "token": state.token,
            "intents": state.intents,
            "properties": { "os": "linux", "browser": "hermes", "device": "hermes" },
        },
    })
}

/// The HEARTBEAT payload (`{"op":1,"d":<last seq or null>}`).
pub fn heartbeat_payload(state: &GatewayState) -> Value {
    json!({ "op": GW_HEARTBEAT, "d": state.seq })
}

/// The RESUME payload (`{"op":6,"d":{token, session_id, seq}}`).
pub fn resume_payload(state: &GatewayState) -> Value {
    json!({
        "op": GW_RESUME,
        "d": {
            "token": state.token,
            "session_id": state.session_id,
            "seq": state.seq,
        },
    })
}

/// Normalize a MESSAGE_CREATE dispatch `d` into the shared inbound contract.
/// Non-text messages (embeds-only, empty content) are dropped, like the Python
/// `on_message` listener skipping them.
pub fn normalize_message_create(d: &Value) -> Option<InboundMessage> {
    let content = d.get("content").and_then(Value::as_str).unwrap_or("");
    if content.is_empty() {
        return None;
    }
    let author = d.get("author")?;
    Some(InboundMessage {
        platform: Platform::Discord,
        chat_id: d.get("channel_id").and_then(Value::as_str)?.to_string(),
        user_id: author.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
        username: author.get("username").and_then(Value::as_str).map(String::from),
        text: content.to_string(),
        message_id: d.get("id").and_then(Value::as_str).unwrap_or("").to_string(),
        thread_id: None, // thread sends target the thread id as chat_id (see rest.rs)
    })
}

// ---------------------------------------------------------------------------
// Connection loop
// ---------------------------------------------------------------------------

/// Send one text frame carrying `payload` (masked, as a client must).
async fn send_text<W: AsyncWrite + Unpin>(w: &mut W, payload: &Value) -> Result<()> {
    let frame = encode_frame(true, OP_TEXT, payload.to_string().as_bytes());
    w.write_all(&frame).await?;
    w.flush().await?;
    Ok(())
}

/// Drive one established gateway connection (post-handshake) to completion.
///
/// The HELLO → IDENTIFY exchange happens first; afterwards heartbeats fire
/// every `heartbeat_interval` and inbound payloads route through
/// [`handle_payload`], with MESSAGE_CREATE dispatches handed to `on_dispatch`.
/// Returns when the peer closes (close frame / EOF / op 7) or when a
/// heartbeat goes un-acked for a full interval (zombie connection) — the
/// caller reconnects, like the Python gateway runner's loop.
pub async fn run_with_stream<IO, F>(
    mut io: IO,
    state: &mut GatewayState,
    mut on_dispatch: F,
) -> Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin,
    F: FnMut(InboundMessage) -> BoxFut<'static, Result<()>>,
{
    // First frame must be HELLO before anything is sent.
    let first = read_frame(&mut io).await?;
    anyhow::ensure!(
        first.opcode == OP_TEXT,
        "expected HELLO text frame, got opcode {}",
        first.opcode
    );
    let hello: Value = serde_json::from_slice(&first.payload)?;
    let action = handle_payload(state, &hello);
    anyhow::ensure!(
        action == GwAction::Identify,
        "first gateway payload did not request identify: {:?}",
        action
    );
    send_text(&mut io, &identify_payload(state)).await?;

    let interval = Duration::from_millis(state.heartbeat_interval_ms.max(1));
    let mut next_heartbeat = tokio::time::Instant::now() + interval;
    // Read frames as they arrive; fire the heartbeat whenever the timer is due.
    loop {
        let remaining = next_heartbeat.saturating_duration_since(tokio::time::Instant::now());
        let frame = if remaining.is_zero() {
            None // heartbeat due
        } else {
            match tokio::time::timeout(remaining, read_frame(&mut io)).await {
                Ok(Ok(f)) => Some(f),
                Ok(Err(e)) => return Err(anyhow!("gateway read failed: {}", e)),
                Err(_elapsed) => None, // heartbeat due
            }
        };
        let Some(frame) = frame else {
            // Heartbeat timer fired: a still-un-acked heartbeat means the link
            // is a zombie (a full interval passed with no ACK) → reconnect.
            anyhow::ensure!(state.heartbeat_acked, "gateway heartbeat not acked");
            send_text(&mut io, &heartbeat_payload(state)).await?;
            state.heartbeat_acked = false;
            next_heartbeat = tokio::time::Instant::now() + interval;
            continue;
        };
        match frame.opcode {
            OP_PING => {
                // MUST answer pings with a pong (RFC6455 5.5.2).
                io.write_all(&encode_frame(true, OP_PONG, &frame.payload)).await?;
                io.flush().await?;
            }
            OP_PONG => {}
            OP_CLOSE => {
                // Echo close, then return so the caller reconnects.
                let _ = io.write_all(&encode_frame(true, OP_CLOSE, &frame.payload)).await;
                let _ = io.flush().await;
                return Err(anyhow!("gateway closed"));
            }
            OP_TEXT | OP_BINARY => {
                let payload: Value = serde_json::from_slice(&frame.payload)?;
                match handle_payload(state, &payload) {
                    GwAction::None => {}
                    GwAction::Identify => send_text(&mut io, &identify_payload(state)).await?,
                    GwAction::Heartbeat => {
                        send_text(&mut io, &heartbeat_payload(state)).await?
                    }
                    GwAction::Resume => send_text(&mut io, &resume_payload(state)).await?,
                    GwAction::Reconnect => return Err(anyhow!("gateway asked to reconnect")),
                    GwAction::Inbound(msg) => on_dispatch(msg).await?,
                }
            }
            _ => {}
        }
    }
}

/// Minimal ws(s) URL split (no URL-crate dependency).
pub struct GatewayAddr {
    pub host: String,
    pub port: u16,
    pub host_header: String,
    pub path: String,
}

pub fn parse_gateway_url(raw: &str) -> Result<GatewayAddr> {
    let (is_tls, rest) = raw
        .strip_prefix("wss://")
        .map(|r| (true, r))
        .or_else(|| raw.strip_prefix("ws://").map(|r| (false, r)))
        .ok_or_else(|| anyhow!("not a ws(s) URL: {}", raw))?;
    let (authority, path) = match rest.find('/') {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, "/"),
    };
    let (host, port) = match authority.rsplit_once(':') {
        Some((h, p)) => (h, p.parse::<u16>()?),
        None => (authority, if is_tls { 443 } else { 80 }),
    };
    Ok(GatewayAddr {
        host: host.to_string(),
        port,
        host_header: authority.to_string(),
        path: path.to_string(),
    })
}

/// Production connector: resolve the gateway URL, open the RFC6455 handshake on
/// a plain tokio `TcpStream` (wss/TLS deferred — module docs), run the loop.
pub async fn connect<F>(token: &str, intents: u64, on_dispatch: F) -> Result<()>
where
    F: FnMut(InboundMessage) -> BoxFut<'static, Result<()>>,
{
    let addr = parse_gateway_url(GATEWAY_URL)?;
    let mut stream = tokio::net::TcpStream::connect((addr.host.as_str(), addr.port)).await?;
    handshake_on_stream(&mut stream, &addr.host_header, &addr.path).await?;
    let mut state = GatewayState::new(token, intents);
    run_with_stream(stream, &mut state, on_dispatch).await
}

/// Send the HTTP/101 upgrade request on an established stream and validate the
/// response; the stream is then a websocket connection.
async fn handshake_on_stream<IO>(mut io: IO, host_header: &str, path: &str) -> Result<()>
where
    IO: AsyncRead + AsyncWrite + Unpin,
{
    // 16 pseudo-random bytes (transport obfuscation only).
    let mut key_bytes = [0u8; 16];
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x5851F42D4C957F2D);
    for (i, b) in key_bytes.iter_mut().enumerate() {
        *b = (seed >> (i % 8)) as u8 ^ (i as u8).wrapping_mul(31);
    }
    let handshake = format!(
        "GET {path} HTTP/1.1\r\nHost: {host}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n",
        path = path,
        host = host_header,
        key = base64_key(&key_bytes),
    );
    io.write_all(handshake.as_bytes()).await?;
    io.flush().await?;
    let mut response = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        io.read_exact(&mut byte).await?;
        response.push(byte[0]);
        if response.ends_with(b"\r\n\r\n") {
            break;
        }
        anyhow::ensure!(response.len() < 16 * 1024, "handshake response too large");
    }
    let text = String::from_utf8_lossy(&response);
    anyhow::ensure!(
        text.starts_with("HTTP/1.1 101") || text.starts_with("HTTP/1.0 101"),
        "websocket upgrade refused: {}",
        text.lines().next().unwrap_or("")
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- RFC6455 framing ----

    #[tokio::test]
    async fn frame_round_trip_unmasked() {
        let raw = encode_frame(false, OP_TEXT, b"hello");
        assert_eq!(raw[0], 0x81); // FIN | text
        assert_eq!(raw[1], 5);
        let mut cursor = std::io::Cursor::new(raw);
        let f = read_frame(&mut cursor).await.unwrap();
        assert_eq!(f.opcode, OP_TEXT);
        assert_eq!(f.payload, b"hello");
        assert!(f.fin);
    }

    #[test]
    fn client_frames_are_masked() {
        let raw = encode_frame(true, OP_TEXT, b"abc");
        assert_eq!(raw[1] & 0x80, 0x80); // mask bit set
        assert_eq!(raw[1] & 0x7F, 3); // payload len
        let key = &raw[2..6];
        let unmasked: Vec<u8> = raw[6..].iter().zip(key).map(|(b, k)| b ^ k).collect();
        assert_eq!(unmasked, b"abc");
    }

    #[test]
    fn frames_over_125_use_16bit_length() {
        let payload = vec![0u8; 300];
        let raw = encode_frame(false, OP_BINARY, &payload);
        assert_eq!(raw[1], 126);
        assert_eq!(u16::from_be_bytes([raw[2], raw[3]]), 300);
        assert_eq!(raw.len(), 4 + 300);
    }

    #[test]
    fn base64_is_rfc4648() {
        assert_eq!(base64_key(&[0; 16]), "AAAAAAAAAAAAAAAAAAAAAA==");
        assert_eq!(base64_key(&[0; 16]).len(), 24);
    }

    #[tokio::test]
    async fn continuation_frames_reassemble() {
        // First frame: opcode TEXT with FIN=0 (a fragmented message start);
        // second: FIN|continuation carrying the tail.
        let mut raw = Vec::new();
        raw.extend_from_slice(&[0x01, 3, b'h', b'e', b'l']);
        raw.extend_from_slice(&encode_frame(false, OP_CONTINUATION, b"lo"));
        let mut cursor = std::io::Cursor::new(raw);
        let f = read_frame(&mut cursor).await.unwrap();
        assert_eq!(f.opcode, OP_TEXT);
        assert_eq!(f.payload, b"hello");
        assert!(f.fin);
    }

    // ---- pure state machine ----

    #[test]
    fn hello_triggers_identify_and_sets_interval() {
        let mut st = GatewayState::new("TOK", DEFAULT_INTENTS);
        let hello = json!({ "op": 10, "d": { "heartbeat_interval": 41250 } });
        assert_eq!(handle_payload(&mut st, &hello), GwAction::Identify);
        assert_eq!(st.heartbeat_interval_ms, 41250);
        let id = identify_payload(&st);
        assert_eq!(id["op"], 2);
        assert_eq!(id["d"]["token"], "TOK");
        assert_eq!(id["d"]["intents"], DEFAULT_INTENTS);
        assert_eq!(id["d"]["properties"]["browser"], "hermes");
    }

    #[test]
    fn dispatch_updates_seq_and_normalizes_message_create() {
        let mut st = GatewayState::new("TOK", 0);
        let dispatch = json!({
            "op": 0, "t": "MESSAGE_CREATE", "s": 42,
            "d": {
                "id": "99", "content": "hello bot",
                "author": { "id": "7", "username": "ada" },
                "channel_id": "555",
            },
        });
        match handle_payload(&mut st, &dispatch) {
            GwAction::Inbound(msg) => {
                assert_eq!(msg.platform, Platform::Discord);
                assert_eq!(msg.chat_id, "555");
                assert_eq!(msg.user_id, "7");
                assert_eq!(msg.username.as_deref(), Some("ada"));
                assert_eq!(msg.text, "hello bot");
                assert_eq!(msg.message_id, "99");
                assert!(msg.thread_id.is_none());
            }
            other => panic!("expected inbound, got {:?}", other),
        }
        assert_eq!(st.seq, Some(42));
    }

    #[test]
    fn ready_captures_session_and_resume_carries_it() {
        let mut st = GatewayState::new("TOK", 0);
        st.seq = Some(43);
        let ready = json!({ "op": 0, "t": "READY", "s": 43, "d": { "session_id": "sess1" } });
        assert_eq!(handle_payload(&mut st, &ready), GwAction::None);
        assert_eq!(st.session_id.as_deref(), Some("sess1"));
        // HELLO after READY (reconnect) → RESUME, not re-identify.
        let hello = json!({ "op": 10, "d": { "heartbeat_interval": 41250 } });
        assert_eq!(handle_payload(&mut st, &hello), GwAction::Resume);
        let resume = resume_payload(&st);
        assert_eq!(resume["op"], 6);
        assert_eq!(resume["d"]["session_id"], "sess1");
        assert_eq!(resume["d"]["seq"], 43);
        assert_eq!(resume["d"]["token"], "TOK");
    }

    #[test]
    fn server_heartbeat_is_echoed_with_current_seq() {
        let mut st = GatewayState::new("TOK", 0);
        st.seq = Some(7);
        assert_eq!(
            handle_payload(&mut st, &json!({ "op": 1, "d": null })),
            GwAction::Heartbeat
        );
        assert_eq!(heartbeat_payload(&st), json!({ "op": 1, "d": 7 }));
        // No sequence yet → null.
        let st2 = GatewayState::new("TOK", 0);
        assert_eq!(heartbeat_payload(&st2), json!({ "op": 1, "d": null }));
    }

    #[test]
    fn reconnect_and_invalid_session_drive_resume_or_reidentify() {
        let mut st = GatewayState::new("TOK", 0);
        st.session_id = Some("s".into());
        st.seq = Some(9);
        assert_eq!(
            handle_payload(&mut st, &json!({ "op": 7, "d": null })),
            GwAction::Reconnect
        );
        // Invalid session (false) → drop session, re-identify.
        assert_eq!(
            handle_payload(&mut st, &json!({ "op": 9, "d": false })),
            GwAction::Identify
        );
        assert!(st.session_id.is_none());
        // Invalid session (true) → resume.
        st.session_id = Some("s".into());
        assert_eq!(handle_payload(&mut st, &json!({ "op": 9, "d": true })), GwAction::Resume);
    }

    #[test]
    fn empty_content_and_unknown_dispatches_are_ignored() {
        let mut st = GatewayState::new("TOK", 0);
        assert_eq!(
            handle_payload(&mut st, &json!({
                "op": 0, "t": "MESSAGE_CREATE", "s": 1,
                "d": { "id": "1", "content": "", "author": { "id": "2" }, "channel_id": "3" },
            })),
            GwAction::None
        );
        assert_eq!(
            handle_payload(&mut st, &json!({ "op": 0, "t": "TYPING_START", "s": 2, "d": {} })),
            GwAction::None
        );
        assert_eq!(handle_payload(&mut st, &json!({ "op": 99 })), GwAction::None);
    }

    #[test]
    fn gateway_url_parses() {
        let a = parse_gateway_url(GATEWAY_URL).unwrap();
        assert_eq!(a.host, "gateway.discord.gg");
        assert_eq!(a.port, 443);
        assert_eq!(a.path, "/?v=10&encoding=json");
        assert_eq!(a.host_header, "gateway.discord.gg");
    }

    // ---- full loop over a stubbed transport ----

    /// Scripted gateway peer over an in-memory duplex stream: queues server→
    /// client frames, records what the client sends. No network, no timers to
    /// race — the loop is driven one flush at a time.
    struct ScriptedPeer {
        io: tokio::io::DuplexStream,
        pending: Vec<u8>,
        sent: Vec<Frame>,
    }

    impl ScriptedPeer {
        fn new(buffer: usize) -> (Self, tokio::io::DuplexStream) {
            let (client, server) = tokio::io::duplex(buffer);
            (
                ScriptedPeer { io: server, pending: Vec::new(), sent: Vec::new() },
                client,
            )
        }

        fn push_json(&mut self, v: &Value) {
            self.push_frame(encode_frame(false, OP_TEXT, v.to_string().as_bytes()));
        }

        fn push_frame(&mut self, frame: Vec<u8>) {
            self.pending.extend_from_slice(&frame);
        }

        /// Deliver every queued frame, then collect what the client answered.
        async fn flush_and_read(&mut self) -> Result<()> {
            let batch = std::mem::take(&mut self.pending);
            if !batch.is_empty() {
                self.io.write_all(&batch).await?;
                self.io.flush().await?;
            }
            // Read frames until the client goes quiet (its loop ended or it is
            // waiting on a heartbeat timer).
            loop {
                match tokio::time::timeout(Duration::from_millis(150), read_frame(&mut self.io))
                    .await
                {
                    Ok(Ok(f)) => self.sent.push(f),
                    Ok(Err(_)) | Err(_) => break,
                }
            }
            Ok(())
        }

        fn text_payloads(&self) -> Vec<Value> {
            self.sent
                .iter()
                .filter(|f| f.opcode == OP_TEXT)
                .map(|f| serde_json::from_slice::<Value>(&f.payload).unwrap())
                .collect()
        }
    }

    #[tokio::test]
    async fn loop_identifies_dispatches_and_echoes_close() {
        let (mut peer, client) = ScriptedPeer::new(64 * 1024);
        let mut st = GatewayState::new("TOK", DEFAULT_INTENTS);
        st.heartbeat_interval_ms = 100_000; // no heartbeat expected in this test

        let dispatched = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let d2 = dispatched.clone();
        let run = tokio::spawn({
            let mut st = st;
            async move {
                let _ = run_with_stream(client, &mut st, move |msg| {
                    let log = d2.clone();
                    Box::pin(async move {
                        log.lock().unwrap().push(msg.text);
                        Ok(())
                    })
                })
                .await;
            }
        });

        peer.push_json(&json!({ "op": 10, "d": { "heartbeat_interval": 100_000 } }));
        peer.push_json(&json!({
            "op": 0, "t": "MESSAGE_CREATE", "s": 1,
            "d": { "id": "9", "content": "ping", "author": { "id": "3", "username": "u" }, "channel_id": "42" },
        }));
        peer.push_frame(encode_frame(false, OP_CLOSE, &[0, 100]));
        let _ = tokio::time::timeout(Duration::from_secs(3), run).await;
        peer.flush_and_read().await.unwrap();

        // The client identified…
        let texts = peer.text_payloads();
        assert_eq!(texts[0]["op"], 2);
        assert_eq!(texts[0]["d"]["token"], "TOK");
        assert_eq!(texts[0]["d"]["intents"], DEFAULT_INTENTS);
        // …surfaced exactly the one MESSAGE_CREATE…
        assert_eq!(*dispatched.lock().unwrap(), vec!["ping".to_string()]);
        // …and echoed the close frame.
        assert!(peer.sent.iter().any(|f| f.opcode == OP_CLOSE));
    }

    #[tokio::test]
    async fn loop_heartbeats_with_tracked_seq() {
        let (mut peer, client) = ScriptedPeer::new(64 * 1024);
        let mut st = GatewayState::new("TOK", 0);
        st.heartbeat_interval_ms = 30; // fast heartbeat so the test observes one
        st.seq = Some(5);

        let run = tokio::spawn(async move {
            let _ = run_with_stream(client, &mut st, |_| Box::pin(async { Ok(()) })).await;
        });

        peer.push_json(&json!({ "op": 10, "d": { "heartbeat_interval": 30 } }));
        // Let a couple of heartbeat intervals pass, then collect.
        tokio::time::sleep(Duration::from_millis(80)).await;
        drop(run);
        peer.flush_and_read().await.unwrap();

        let texts = peer.text_payloads();
        let beats: Vec<&Value> = texts.iter().filter(|v| v["op"] == 1).collect();
        assert!(!beats.is_empty(), "no heartbeat sent within 80ms");
        assert!(
            beats.iter().all(|v| v["d"] == 5),
            "heartbeat carried the wrong sequence: {:?}",
            beats
        );
    }

    #[tokio::test]
    async fn server_ping_gets_pong() {
        let (mut peer, client) = ScriptedPeer::new(16 * 1024);
        let mut st = GatewayState::new("TOK", 0);
        st.heartbeat_interval_ms = 100_000;
        let run = tokio::spawn(async move {
            let _ = run_with_stream(client, &mut st, |_| Box::pin(async { Ok(()) })).await;
        });
        peer.push_json(&json!({ "op": 10, "d": { "heartbeat_interval": 100_000 } }));
        peer.push_frame(encode_frame(false, OP_PING, b"probe"));
        tokio::time::sleep(Duration::from_millis(120)).await;
        drop(run);
        peer.flush_and_read().await.unwrap();
        assert!(
            peer.sent.iter().any(|f| f.opcode == OP_PONG && f.payload == b"probe"),
            "no pong with the ping payload"
        );
    }

    #[tokio::test]
    async fn reconnect_op_ends_the_loop() {
        let (mut peer, client) = ScriptedPeer::new(16 * 1024);
        let mut st = GatewayState::new("TOK", 0);
        st.heartbeat_interval_ms = 100_000;
        let run = tokio::spawn(async move {
            let r = run_with_stream(client, &mut st, |_| Box::pin(async { Ok(()) })).await;
            assert!(r.is_err()); // reconnect ⇒ error return, caller reconnects
        });
        peer.push_json(&json!({ "op": 10, "d": { "heartbeat_interval": 100_000 } }));
        peer.push_json(&json!({ "op": 7, "d": null }));
        let _ = tokio::time::timeout(Duration::from_secs(3), run).await;
    }
}
