//! execute_code — Programmatic Tool Calling (port of
//! `tools/code_execution_tool.py` + `tools/code_kernel.py`).
//!
//! The LLM writes a Python script that calls Hermes tools via RPC, collapsing
//! multi-step tool chains into one inference turn; only the script's stdout
//! returns to the LLM. Local backend: a persistent per-owner session kernel
//! (Python `tools/code_kernel.py`) over loopback TCP (Windows parity — Python
//! uses a UDS elsewhere). The sandbox-side Python sources (the generated
//! `hermes_tools.py` stub module and the kernel runner) are embedded here
//! verbatim and staged into the kernel's tmpdir at spawn.
//!
//! Wire protocol (Python `code_kernel.py`): one JSON request per stdin line
//! `{"id", "code"}`; replies framed on stdout as
//! `\n<SENTINEL> <byte-length>\n<json>` with a per-kernel random SENTINEL.
//! Bytes outside frames are raw fd output attributed to the running cell.
//!
//! The kernel child is injected as the `KernelSpawner`/`KernelChild` seams:
//! production spawns the embedded runner under Python; tests stub the child
//! (no live interpreter). Tool RPC back into the registry is served by
//! `rpc_server_loop` (loopback TCP, token + whitelist + per-cell budget).
//!
//! Deferred from the Python original: LRU kernel caps / idle reaping, the
//! remote terminal-backend paths (`code_kernel_remote` + per-call ship), and
//! the approval/lifecycle guards — those live in host layers this crate
//! deliberately has no dependency on.

use regex::Regex;
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, Instant};

use crate::util::tool_error;

pub const DEFAULT_TIMEOUT: u64 = 300; // 5 minutes
pub const DEFAULT_MAX_TOOL_CALLS: u64 = 50;
pub const MAX_STDOUT_BYTES: usize = 50_000; // 50 KB
pub const MAX_STDERR_BYTES: usize = 10_000; // 10 KB
/// Hard ceiling on the spilled file: a runaway print loop must not fill the disk.
pub const MAX_SPILLED_STDOUT_BYTES: usize = 5_000_000;
/// Runner-side cap on captured python-level output (`_RUNNER_CAPTURE_BYTES`).
pub const RUNNER_CAPTURE_BYTES: usize = 1_000_000;

/// Tools allowed inside the sandbox; ∩ the session's enabled tools decides
/// which stubs are generated.
pub const SANDBOX_ALLOWED_TOOLS: &[&str] = &[
    "web_search",
    "web_extract",
    "read_file",
    "write_file",
    "search_files",
    "patch",
    "terminal",
];

// ---- hermes_tools.py code generator (Python `_TOOL_STUBS`) -----------------
// (signature, docstring, args_dict_expr — the JSON payload sent over RPC).

const TOOL_STUBS: &[(&str, &str, &str, &str)] = &[
    (
        "web_search",
        "query: str, limit: int = 5",
        "\"\"\"Search the web. Returns dict with data.web list of {url, title, description}.\"\"\"",
        "{\"query\": query, \"limit\": limit}",
    ),
    (
        "web_extract",
        "urls: list, char_limit: int = None",
        "\"\"\"Extract content from URLs (no LLM summarization). Returns dict with results list of {url, title, content, error}. Pages over char_limit (default 15000) are head+tail truncated with the full text stored on disk; the content footer gives the path. content is markdown.\"\"\"",
        "{\"urls\": urls, \"char_limit\": char_limit}",
    ),
    (
        "read_file",
        "path: str, offset: int = 1, limit: int = 2000",
        "\"\"\"Read a file (1-indexed lines). Returns dict with \"content\" and \"total_lines\".\"\"\"",
        "{\"path\": path, \"offset\": offset, \"limit\": limit}",
    ),
    (
        "write_file",
        "path: str, content: str, cross_profile: bool = False",
        "\"\"\"Write content to a file (always overwrites). Returns dict with status.\"\"\"",
        "{\"path\": path, \"content\": content, \"cross_profile\": cross_profile}",
    ),
    (
        "search_files",
        "pattern: str, target: str = \"content\", path: str = \".\", file_glob: str = None, limit: int = 50, offset: int = 0, output_mode: str = \"content\", context: int = 0, order: str = \"discovery\"",
        "\"\"\"Search file contents (target=\"content\") or find files by name (target=\"files\"). Returns dict with \"matches\".\"\"\"",
        "{\"pattern\": pattern, \"target\": target, \"path\": path, \"file_glob\": file_glob, \"limit\": limit, \"offset\": offset, \"output_mode\": output_mode, \"context\": context, \"order\": order}",
    ),
    (
        "patch",
        "path: str = None, old_string: str = None, new_string: str = None, replace_all: bool = False, mode: str = \"replace\", patch: str = None, cross_profile: bool = False",
        "\"\"\"Targeted find-and-replace (mode=\"replace\") or V4A multi-file patches (mode=\"patch\"). Returns dict with status.\"\"\"",
        "{\"path\": path, \"old_string\": old_string, \"new_string\": new_string, \"replace_all\": replace_all, \"mode\": mode, \"patch\": patch, \"cross_profile\": cross_profile}",
    ),
    (
        "terminal",
        "command: str, timeout: int = None, workdir: str = None",
        "\"\"\"Run a shell command (foreground only). Returns dict with \"output\" and \"exit_code\".\"\"\"",
        "{\"command\": command, \"timeout\": timeout, \"workdir\": workdir}",
    ),
];

// ---- Shared helpers section (embedded in both transport headers) -----------

const COMMON_HELPERS: &str = r#"

# ---------------------------------------------------------------------------
# Convenience helpers (avoid common scripting pitfalls)
# ---------------------------------------------------------------------------

def json_parse(text: str):
    """Parse JSON tolerant of control characters and UTF-8 BOM (strict=False).
    Use this instead of json.loads() when parsing output from terminal()
    or web_extract() that may contain raw tabs/newlines in strings,
    or from tools/files that prepend a UTF-8 BOM (salvage #57870, credit @woxinwuhen713-bit)."""
    if isinstance(text, str) and text.startswith("\ufeff"):
        text = text[1:]
    return json.loads(text, strict=False)


def shell_quote(s: str) -> str:
    """Shell-escape a string for safe interpolation into commands.
    Use this when inserting dynamic content into terminal() commands:
        terminal(f"echo {shell_quote(user_input)}")
    """
    return shlex.quote(s)


def retry(fn, max_attempts=3, delay=2):
    """Retry a function up to max_attempts times with exponential backoff.
    Use for transient failures (network errors, API rate limits):
        result = retry(lambda: terminal("gh issue list ..."))
    """
    last_err = None
    for attempt in range(max_attempts):
        try:
            return fn()
        except Exception as e:
            last_err = e
            if attempt < max_attempts - 1:
                time.sleep(delay * (2 ** attempt))
    raise last_err
"#;

// ---- UDS transport (local backend) -----------------------------------------

const UDS_TRANSPORT_HEADER: &str = r#""""Auto-generated Hermes tools RPC stubs."""
import json, os, socket, shlex, threading, time

_sock = None
# The RPC server handles a single client connection serially and has no
# request-id in the protocol, so concurrent _call() invocations from multiple
# threads (e.g. ThreadPoolExecutor) would race on the shared socket and get
# each other's responses. Serialize the entire send+recv round-trip.
_call_lock = threading.Lock()
"#;

const UDS_CONNECT_AND_CALL: &str = r#"

def _connect():
    """Connect to the parent's RPC server via the transport it picked.

    HERMES_RPC_SOCKET can be either:
      - a filesystem path (POSIX Unix domain socket — the default on
        Linux and macOS)
      - a string of the form ``tcp://127.0.0.1:<port>`` (Windows, where
        AF_UNIX is unreliable — the parent falls back to loopback TCP)
    """
    global _sock
    if _sock is None:
        endpoint = os.environ["HERMES_RPC_SOCKET"]
        if endpoint.startswith("tcp://"):
            _host_port = endpoint[len("tcp://"):]
            _host, _, _port = _host_port.rpartition(":")
            _sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            _sock.connect((_host or "127.0.0.1", int(_port)))
        else:
            _sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
            _sock.connect(endpoint)
        _sock.settimeout(300)
    return _sock

def _call(tool_name, args):
    """Send a tool call to the parent process and return the parsed result."""
    request = json.dumps({
        "tool": tool_name,
        "args": args,
        "token": os.environ.get("HERMES_RPC_TOKEN", ""),
    }) + "\n"
    # Session kernels outlive the RPC server's 300s idle window, so their
    # connection can be legitimately gone by the next cell. The server
    # re-accepts (HERMES_RPC_PERSISTENT=1); retry once on a fresh socket.
    _attempts = 2 if os.environ.get("HERMES_RPC_PERSISTENT") == "1" else 1
    with _call_lock:
        for _attempt in range(_attempts):
            try:
                conn = _connect()
                conn.sendall(request.encode())
                buf = b""
                while True:
                    chunk = conn.recv(65536)
                    if not chunk:
                        raise RuntimeError("Agent process disconnected")
                    buf += chunk
                    if buf.endswith(b"\n"):
                        break
                break
            except (OSError, RuntimeError):
                global _sock
                try:
                    if _sock is not None:
                        _sock.close()
                except OSError:
                    pass
                _sock = None
                if _attempt + 1 >= _attempts:
                    raise
    raw = buf.decode().strip()
    result = json.loads(raw)
    if isinstance(result, str):
        try:
            return json.loads(result)
        except (json.JSONDecodeError, TypeError):
            return result
    return result
"#;

// ---- File-based transport (remote backends) --------------------------------

const FILE_TRANSPORT_HEADER: &str = r#""""Auto-generated Hermes tools RPC stubs (file-based transport)."""
import json, os, shlex, tempfile, threading, time

_RPC_DIR = os.environ.get("HERMES_RPC_DIR") or os.path.join(tempfile.gettempdir(), "hermes_rpc")
_seq = 0
# `_seq += 1` is not atomic (read-modify-write), so concurrent _call()
# invocations from multiple threads could allocate the same sequence number
# and clobber each other's request files. Guard seq allocation with a lock.
_seq_lock = threading.Lock()
"#;

const FILE_CALL: &str = r#"

def _call(tool_name, args):
    """Send a tool call request via file-based RPC and wait for response."""
    global _seq
    with _seq_lock:
        _seq += 1
        seq = _seq
    seq_str = f"{seq:06d}"
    req_file = os.path.join(_RPC_DIR, f"req_{seq_str}")
    res_file = os.path.join(_RPC_DIR, f"res_{seq_str}")

    # Write request atomically (write to .tmp, then rename).
    # encoding="utf-8" is critical: on Windows-hosted remote backends
    # (or any non-UTF-8 locale) the default open() mode would mangle
    # non-ASCII chars in tool args when encoding them as JSON.
    tmp = req_file + ".tmp"
    fd = os.open(tmp, os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        json.dump({
            "tool": tool_name,
            "args": args,
            "seq": seq,
            "token": os.environ.get("HERMES_RPC_TOKEN", ""),
        }, f)
    os.rename(tmp, req_file)

    # Wait for response with adaptive polling
    deadline = time.monotonic() + 300  # 5-minute timeout per tool call
    poll_interval = 0.05  # Start at 50ms
    while not os.path.exists(res_file):
        if time.monotonic() > deadline:
            raise RuntimeError(f"RPC timeout: no response for {tool_name} after 300s")
        time.sleep(poll_interval)
        poll_interval = min(poll_interval * 1.2, 0.25)  # Back off to 250ms

    with open(res_file, encoding="utf-8") as f:
        raw = f.read()

    try:
        os.unlink(res_file)
    except OSError:
        pass

    result = json.loads(raw)
    if isinstance(result, str):
        try:
            return json.loads(result)
        except (json.JSONDecodeError, TypeError):
            return result
    return result
"#;

/// Source of the hermes_tools.py stub module for
/// SANDBOX_ALLOWED_TOOLS ∩ `enabled_tools`. `transport`: "uds" (local socket
/// client) or "file" (file RPC, remote backends).
pub fn generate_hermes_tools_module(enabled_tools: &[&str], transport: &str) -> String {
    let (header, call) = if transport == "file" {
        (FILE_TRANSPORT_HEADER, FILE_CALL)
    } else {
        (UDS_TRANSPORT_HEADER, UDS_CONNECT_AND_CALL)
    };
    let mut stubs: Vec<&(&str, &str, &str, &str)> = TOOL_STUBS
        .iter()
        .filter(|(name, ..)| enabled_tools.contains(&name))
        .collect();
    stubs.sort_by(|a, b| a.0.cmp(b.0));
    let body: Vec<String> = stubs
        .iter()
        .map(|(name, sig, doc, args)| {
            format!("def {name}({sig}):\n    {doc}\n    return _call('{name}', {args})\n")
        })
        .collect();
    format!("{header}{COMMON_HELPERS}{call}\n{}", body.join("\n"))
}

// ---- Kernel runner (Python `KERNEL_RUNNER_SOURCE`) --------------------------
// One exec cell per request; frames on stdout as
// `\n<SENTINEL> <byte-length>\n<json>`. Bytes outside frames are raw fd output.

const KERNEL_RUNNER_SOURCE: &str = r#""""Auto-generated Hermes session-kernel runner. One exec cell per request."""
import contextlib
import io
import json
import os
import sys
import threading
import traceback

_SENTINEL = os.environ["HERMES_KERNEL_SENTINEL"]
_CAPTURE_LIMIT = 1000000
_SPILL_DIR = os.environ.get("HERMES_KERNEL_SPILL_DIR", "")
_SPILL_CAP = 5000000

_real_stdout = sys.stdout

GLOBALS = {"__name__": "__main__", "__builtins__": __builtins__}


def _clip(text):
    return (text, False) if len(text) <= _CAPTURE_LIMIT else (text[:_CAPTURE_LIMIT], True)


def run_cell(request, execution_count):
    """Exec one cell; returns (response payload, FULL stdout text)."""
    out, err = io.StringIO(), io.StringIO()
    status, trace = "ok", ""
    try:
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(err):
            exec(compile(request["code"], "<cell>", "exec"), GLOBALS)
    except SystemExit as exc:
        status, trace = "exit", "SystemExit: " + repr(exc.code)
    except BaseException:
        status, trace = "error", traceback.format_exc()
    stdout_text, stdout_clipped = _clip(out.getvalue())
    stderr_text, stderr_clipped = _clip(err.getvalue())
    return {
        "id": request.get("id", ""), "status": status,
        "stdout": stdout_text, "stderr": stderr_text,
        "stdout_clipped": stdout_clipped, "stderr_clipped": stderr_clipped,
        "traceback": trace, "execution_count": execution_count,
    }, out.getvalue()


def _spill(text, spill_name):
    """Best-effort: write the FULL clipped stdout to disk, return its path or ""."""
    if not _SPILL_DIR:
        return ""
    try:
        spill_path = os.path.join(_SPILL_DIR, spill_name)
        with open(spill_path, "w", encoding="utf-8", errors="replace") as f:
            f.write(text[:_SPILL_CAP])
            if len(text) > _SPILL_CAP:
                f.write("\n\n[... spill capped ...]")
        return spill_path
    except Exception:
        return ""


def _reply(payload):
    body = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    _real_stdout.buffer.write(("\n" + _SENTINEL + " " + str(len(body)) + "\n").encode("utf-8"))
    _real_stdout.buffer.write(body)
    _real_stdout.buffer.flush()


def main():
    execution_count = 0
    for line in sys.stdin:
        line = line.strip()
        if not line:
            continue
        try:
            request = json.loads(line)
        except ValueError:
            continue
        execution_count += 1
        payload, full_stdout = run_cell(request, execution_count)
        payload["stdout_spill_path"] = (
            _spill(full_stdout, "cell_%06d_stdout.txt" % execution_count)
            if payload["stdout_clipped"] else ""
        )
        _reply(payload)
        if payload["status"] == "exit":
            break


if __name__ == "__main__":
    main()
"#;

// ---- Well-known failure hints (Python `_FAILURE_HINT_RULES`) ---------------

/// Map well-known sandbox script failures to one actionable recovery hint
/// (bounded scan, first match wins).
pub fn sandbox_failure_hint(stderr_text: &str) -> Option<String> {
    if stderr_text.is_empty() {
        return None;
    }
    let window: String = stderr_text.chars().take(4000).collect();
    let missing_import = Regex::new(r"cannot import name '(\w+)' from 'hermes_tools'").unwrap();
    let name_error =
        Regex::new(r"NameError: name '(json_parse|shell_quote|retry)' is not defined").unwrap();
    let module_missing = Regex::new(r"ModuleNotFoundError: No module named '([\w.]+)'").unwrap();
    let string_indices = Regex::new(
        r"TypeError: string indices must be integers|AttributeError: 'str' object has no attribute 'get'",
    )
    .unwrap();
    if let Some(m) = missing_import.captures(&window) {
        let missing = m.get(1).unwrap().as_str();
        let hint = if ["json_parse", "shell_quote", "retry"].contains(&missing) {
            format!(
                "Import helpers with `from hermes_tools import {missing}`. If that import \
                 failed, the generated module may be stale or another hermes_tools may be \
                 first on sys.path. Check hermes_tools.__file__ and retry with reset=true."
            )
        } else {
            format!(
                "'{missing}' is not available inside the execute_code sandbox. Importable \
                 tools here: {}. For anything else, use the normal tool call instead of \
                 execute_code.",
                SANDBOX_ALLOWED_TOOLS.join(", ")
            )
        };
        return Some(hint);
    }
    if let Some(m) = name_error.captures(&window) {
        let missing = m.get(1).unwrap().as_str();
        return Some(format!(
            "Import {missing} before calling it: from hermes_tools import {missing}"
        ));
    }
    if let Some(m) = module_missing.captures(&window) {
        let module = m.get(1).unwrap().as_str();
        return Some(format!(
            "'{module}' is not installed in the sandbox interpreter. Use Python stdlib inside \
             execute_code, or run the code via terminal() with the project venv's python \
             instead."
        ));
    }
    if string_indices.is_match(&window) {
        return Some(
            "Tool functions in the sandbox return DICTS (already parsed) — do not \
             json.loads() them or index them like strings. Example: read_file(path)['content']."
                .into(),
        );
    }
    None
}

// ---- Output pipeline ---------------------------------------------------------

/// Python `_truncate_stdout_text`: cap stdout by bytes (40% head / 60% tail)
/// with explicit truncation metadata; the omitted middle is spilled to
/// cache/exec and the result carries the path (recover-don't-rerun).
pub fn truncate_stdout_text(stdout_text: &str) -> (String, Value) {
    let stdout_bytes = stdout_text.as_bytes();
    let total = stdout_bytes.len();
    let captured = total.min(MAX_STDOUT_BYTES);
    let mut metadata = json!({
        "stdout_truncated": total > captured,
        "stdout_bytes_captured": captured,
        "stdout_bytes_total": total,
        "stdout_bytes_omitted": total - captured,
    });
    if total <= MAX_STDOUT_BYTES {
        return (stdout_text.to_string(), metadata);
    }
    let head_bytes = MAX_STDOUT_BYTES * 40 / 100;
    let tail_bytes = MAX_STDOUT_BYTES - head_bytes;
    let head = String::from_utf8_lossy(&stdout_bytes[..head_bytes]).to_string();
    let tail = String::from_utf8_lossy(&stdout_bytes[total - tail_bytes..]).to_string();
    let text = format!(
        "{head}\n\n… [truncated {} of {} bytes] …\n\n{tail}",
        total - captured,
        total
    );
    metadata["warning"] = json!(
        "execute_code stdout was truncated; the script did run, but only the captured \
         head/tail output is included. Re-run only with narrower output if the omitted data \
         is required."
    );
    if let Some(spill_path) = spill_full_stdout(stdout_text) {
        metadata["stdout_spill_path"] = json!(spill_path);
        metadata["warning"] = json!(format!(
            "execute_code stdout was truncated (head/tail shown); the script did run. FULL \
             output saved to {spill_path} — page it with read_file(path=\"{spill_path}\", \
             offset=...) instead of re-running."
        ));
    }
    (text, metadata)
}

/// Python `_spill_full_stdout`: write full stdout to cache/exec, keyed by a
/// content digest so identical reruns coalesce (None on failure — best effort).
fn spill_full_stdout(stdout_text: &str) -> Option<String> {
    let mut text = stdout_text.to_string();
    if stdout_text.len() > MAX_SPILLED_STDOUT_BYTES {
        let mut capped = stdout_text[..MAX_SPILLED_STDOUT_BYTES].to_string();
        capped.push_str(&format!(
            "\n\n[... spill capped at {} bytes ...]",
            MAX_SPILLED_STDOUT_BYTES
        ));
        text = capped;
    }
    let cache_dir = match std::env::var("HERMES_HOME")
        .ok()
        .filter(|h| !h.trim().is_empty())
    {
        Some(home) => std::path::PathBuf::from(home).join("cache").join("exec"),
        None => {
            let mut base = std::env::temp_dir();
            base.push("hermes_cache_exec");
            base
        }
    };
    std::fs::create_dir_all(&cache_dir).ok()?;
    let path = cache_dir.join(format!("stdout-{:012x}.txt", fnv_digest(text.as_bytes())));
    std::fs::write(&path, text).ok()?;
    Some(path.to_string_lossy().to_string())
}

/// FNV-1a content digest (a stable key; identical reruns coalesce).
fn fnv_digest(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in bytes {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

// ---- Tool RPC server (Python `code_execution_rpc._rpc_server_loop`) --------

/// Serve ONE client connection serially: JSON request lines
/// `{"tool", "args", "token"}`; replies are one JSON line. The token must
/// match, the tool must be whitelisted, and the per-cell call budget is
/// enforced. Returns when the client disconnects or `stop` flips.
pub fn rpc_server_loop(
    stream: &mut std::net::TcpStream,
    dispatch: &dyn Fn(&str, &Value) -> String,
    token: &str,
    sandbox_tools: &[&str],
    max_tool_calls: u64,
    counter: &Mutex<u64>,
    log: &Mutex<Vec<Value>>,
    stop: &AtomicBool,
) {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(300)));
    let mut buf: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 65536];
    loop {
        if stop.load(Ordering::Relaxed) {
            return;
        }
        // Read one newline-terminated request.
        let line_end = loop {
            if let Some(pos) = buf.iter().position(|&b| b == b'\n') {
                break pos;
            }
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => return, // disconnected
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        };
        let line: Vec<u8> = buf.drain(..=line_end).collect();
        let Ok(line) = String::from_utf8(line) else {
            continue;
        };
        let request: Value = match serde_json::from_str(line.trim_end_matches('\n')) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let reply = if request.get("token").and_then(Value::as_str).unwrap_or("") != token {
            json!({ "error": "RPC token mismatch" })
        } else {
            let tool = request.get("tool").and_then(Value::as_str).unwrap_or("");
            let args = request.get("args").cloned().unwrap_or(Value::Null);
            if !sandbox_tools.contains(&tool) {
                json!({ "error": format!(
                    "'{tool}' is not available inside the execute_code sandbox. Importable \
                     tools here: {}.",
                    sandbox_tools.join(", ")
                )})
            } else {
                let mut budget = counter.lock().unwrap();
                if *budget >= max_tool_calls {
                    json!({ "error": format!(
                        "Tool call budget exhausted ({max_tool_calls} calls). Return what you \
                         have or finish the script."
                    )})
                } else {
                    *budget += 1;
                    drop(budget);
                    log.lock().unwrap().push(json!({
                        "tool": tool, "args": args,
                        "ts": std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_millis() as u64)
                            .unwrap_or(0),
                    }));
                    let result = dispatch(tool, &args);
                    serde_json::from_str::<Value>(&result).unwrap_or_else(|_| json!(result))
                }
            }
        };
        if stream
            .write_all(
                format!("{}\n", serde_json::to_string(&reply).unwrap_or_default()).as_bytes(),
            )
            .is_err()
        {
            return;
        }
    }
}

// ---- Kernel process seams -----------------------------------------------------

/// What the host stages into the kernel's tmpdir before spawning.
pub struct KernelStaging {
    pub dir: std::path::PathBuf,
    pub hermes_tools_py: String,
    pub runner_py: String,
    pub rpc_endpoint: String,
    pub rpc_token: String,
    pub sentinel: String,
}

/// The kernel child: JSON request lines on stdin, sentinel-framed replies on
/// stdout, raw passthrough and stderr captured alongside.
pub trait KernelChild: Send {
    fn write_cell(&mut self, id: &str, code: &str) -> Result<(), String>;
    /// Wait for the next frame reply (or deadline). Raw bytes read before the
    /// frame belong to the running cell (`take_raw`).
    fn read_reply(&mut self, deadline: Instant) -> Result<Value, String>;
    fn take_raw(&mut self) -> String;
    fn take_stderr(&mut self) -> String;
    fn alive(&mut self) -> bool;
    fn kill(&mut self);
}

/// The spawn seam: production = the embedded runner under Python; tests stub.
pub trait KernelSpawner: Send + Sync {
    fn spawn(&self, staging: &KernelStaging) -> Result<Box<dyn KernelChild>, String>;
}

static SPAWNER: RwLock<Option<Arc<dyn KernelSpawner>>> = RwLock::new(None);

/// Install the kernel spawner (hosts, tests). None restores the default
/// Python child.
pub fn set_kernel_spawner(spawner: Option<Arc<dyn KernelSpawner>>) {
    *SPAWNER.write().unwrap() = spawner;
}

/// One live kernel: child + identity + counters (Python `SessionKernel`,
/// minus LRU/idle lifecycle which the host layers own here).
struct LiveKernel {
    child: Box<dyn KernelChild>,
    execution_count: u64,
    tool_counter: Arc<Mutex<u64>>,
}

static KERNELS: Mutex<Option<std::collections::HashMap<String, LiveKernel>>> = Mutex::new(None);

/// Production child: `python <staged runner>`, env carries the sentinel, the
/// spill dir and the RPC endpoint/token (Python `_spawn`).
struct PythonChild {
    proc: std::process::Child,
    sentinel: String,
    /// Reader-thread bytes: frames + raw passthrough, in arrival order.
    rx: mpsc::Receiver<Vec<u8>>,
    buffer: Vec<u8>,
    raw: Vec<u8>,
    stderr: Vec<u8>,
}

impl PythonChild {
    /// Pull more child bytes into `buffer` until data arrives or the deadline
    /// passes. Ok(()) = data may be present; Err = deadline/EOF with the
    /// reason.
    fn read_more(&mut self, deadline: Instant) -> Result<(), &'static str> {
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err("timeout");
            }
            match self
                .rx
                .recv_timeout(remaining.min(Duration::from_millis(50)))
            {
                Ok(bytes) => {
                    self.buffer.extend_from_slice(&bytes);
                    return Ok(());
                }
                Err(mpsc::RecvTimeoutError::Timeout) => continue,
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    // stdout closed: the child is gone. If a (complete) frame
                    // is still buffered it gets consumed at the loop top;
                    // otherwise the leftovers are raw cell output and the cell
                    // ends `kernel-eof` (Python's reader semantics).
                    if self.buffer.is_empty() {
                        return Err("kernel-eof");
                    }
                    let leftover = std::mem::take(&mut self.buffer);
                    self.raw.extend_from_slice(&leftover);
                    return Err("kernel-eof");
                }
            }
        }
    }
}

/// Split one complete frame out of `buf`: `\n<marker><len>\n<json>`. Bytes
/// before the frame start are left in `buf` (raw passthrough).
fn parse_frame(buf: &mut Vec<u8>, marker: &str) -> Option<Value> {
    let marker_bytes = marker.as_bytes();
    let start = buf
        .windows(marker_bytes.len())
        .position(|w| w == marker_bytes)?;
    let rest = &buf[start + marker_bytes.len()..];
    let newline = rest.iter().position(|&b| b == b'\n')?;
    let length: usize = std::str::from_utf8(&rest[..newline])
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if rest.len() < newline + 1 + length {
        return None;
    }
    let body = &rest[newline + 1..newline + 1 + length];
    let payload: Value = serde_json::from_slice(body).ok()?;
    // Consume only the frame; anything before it stays buffered — it belongs
    // to the running cell (`KernelChild::take_raw`).
    buf.drain(start..start + marker_bytes.len() + newline + 1 + length);
    Some(payload)
}

impl KernelChild for PythonChild {
    fn write_cell(&mut self, id: &str, code: &str) -> Result<(), String> {
        let line = json!({ "id": id, "code": code }).to_string();
        let stdin = self.proc.stdin.as_mut().ok_or("kernel stdin closed")?;
        stdin
            .write_all(format!("{line}\n").as_bytes())
            .map_err(|e| format!("kernel stdin write failed: {e}"))?;
        stdin
            .flush()
            .map_err(|e| format!("kernel stdin flush failed: {e}"))
    }

    fn read_reply(&mut self, deadline: Instant) -> Result<Value, String> {
        let marker = format!("\n{} ", self.sentinel);
        loop {
            if let Some(payload) = parse_frame(&mut self.buffer, &marker) {
                // Anything before the frame start is the running cell's raw
                // output (Python attributes it to the running cell).
                self.raw = std::mem::take(&mut self.buffer);
                return Ok(payload);
            }
            self.read_more(deadline)
                .map_err(|reason| reason.to_string())?;
        }
    }

    fn take_raw(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.raw)).to_string()
    }
    fn take_stderr(&mut self) -> String {
        String::from_utf8_lossy(&std::mem::take(&mut self.stderr)).to_string()
    }
    fn alive(&mut self) -> bool {
        matches!(self.proc.try_wait(), Ok(None))
    }
    fn kill(&mut self) {
        let _ = self.proc.kill();
        let _ = self.proc.wait();
    }
}

/// Default spawner: stage the embedded Python sources and run them with the
/// session interpreter (Python `_spawn`).
pub struct PythonSpawner {
    pub child_python: String,
    pub child_cwd: Option<std::path::PathBuf>,
}

impl KernelSpawner for PythonSpawner {
    fn spawn(&self, staging: &KernelStaging) -> Result<Box<dyn KernelChild>, String> {
        std::fs::create_dir_all(&staging.dir).map_err(|e| format!("kernel tmpdir: {e}"))?;
        std::fs::write(
            staging.dir.join("hermes_tools.py"),
            &staging.hermes_tools_py,
        )
        .map_err(|e| e.to_string())?;
        std::fs::write(
            staging.dir.join("hermes_kernel_runner.py"),
            &staging.runner_py,
        )
        .map_err(|e| e.to_string())?;
        let mut cmd = std::process::Command::new(&self.child_python);
        cmd.arg(staging.dir.join("hermes_kernel_runner.py"))
            .env("HERMES_KERNEL_SENTINEL", &staging.sentinel)
            .env("HERMES_KERNEL_SPILL_DIR", &staging.dir)
            .env("HERMES_RPC_SOCKET", &staging.rpc_endpoint)
            .env("HERMES_RPC_TOKEN", &staging.rpc_token)
            .env("HERMES_RPC_PERSISTENT", "1")
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        if let Some(cwd) = &self.child_cwd {
            cmd.current_dir(cwd);
        }
        let mut proc = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn the session kernel ({e}); is python on PATH?"))?;
        let stdout = proc.stdout.take().ok_or("kernel stdout missing")?;
        let (tx, rx) = mpsc::channel::<Vec<u8>>();
        // One reader thread owns the blocking stdout handle; `read_reply`
        // polls the channel so the cell deadline stays enforceable.
        std::thread::spawn(move || {
            let mut stream = stdout;
            let mut chunk = [0u8; 8192];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(chunk[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Ok(Box::new(PythonChild {
            proc,
            sentinel: staging.sentinel.clone(),
            rx,
            buffer: vec![],
            raw: vec![],
            stderr: vec![],
        }))
    }
}

// ---- Tool entry point ----------------------------------------------------------

/// Execute `execute_code` — the registry entry point.
pub fn execute(args: &Value) -> String {
    // Redirect misdirected calls (terminal's `command` arg, non-string code).
    if !args.get("code").is_some_and(Value::is_string) && args.get("command").is_some() {
        return tool_error(
            "execute_code received a 'command' parameter, but it requires Python source in \
             'code'. Use terminal(command=...) for shell commands; for Python, retry as \
             execute_code(code=\"...\").",
        );
    }
    let code_value = args.get("code").cloned();
    if let Some(not_string) = code_value.as_ref().filter(|c| !c.is_string()) {
        return tool_error(format!(
            "execute_code received a {} in 'code', but it requires Python source as a string. \
             Retry as execute_code(code=\"...\").",
            json_type_name(not_string)
        ));
    }
    let code = code_value
        .and_then(|c| c.as_str().map(str::to_string))
        .unwrap_or_default();
    if code.trim().is_empty() {
        return tool_error(
            "No code provided. execute_code requires a non-empty 'code' parameter containing \
             Python source. To run shell commands, use terminal(command=...) instead.",
        );
    }
    let reset = args.get("reset").and_then(Value::as_bool).unwrap_or(false);
    let owner = args
        .get("task_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|t| !t.is_empty())
        .unwrap_or("default")
        .to_string();
    let timeout = args
        .get("timeout")
        .and_then(Value::as_u64)
        .unwrap_or(DEFAULT_TIMEOUT)
        .clamp(1, 1800);
    run_cell(code, &owner, reset, timeout, DEFAULT_MAX_TOOL_CALLS)
}

fn json_type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "None",
        Value::Bool(_) => "bool",
        Value::Number(_) => "int",
        Value::Array(_) => "list",
        Value::Object(_) => "dict",
        _ => "str",
    }
}

/// Python `execute_in_session_kernel`: acquire (or reuse) the owner's kernel,
/// write the cell, await the reply, assemble the tool result.
fn run_cell(code: String, owner: &str, reset: bool, timeout: u64, _max_tool_calls: u64) -> String {
    let exec_start = Instant::now();
    let sandbox_tools: Vec<&str> = SANDBOX_ALLOWED_TOOLS.to_vec();
    let spawner = SPAWNER.read().unwrap().clone().unwrap_or_else(|| {
        Arc::new(PythonSpawner {
            child_python: "python".into(),
            child_cwd: None,
        })
    });

    // Acquire or respawn the owner's kernel (Python `_acquire_kernel`, minus
    // LRU/idle lifecycle).
    let acquired = with_kernels(|map| {
        if let Some(mut existing) = map.remove(owner) {
            if existing.child.alive() && !reset {
                return Ok((existing, false));
            }
            existing.child.kill();
        }
        let staging = new_staging(&sandbox_tools);
        match spawner.spawn(&staging) {
            Ok(child) => Ok((
                LiveKernel {
                    child,
                    execution_count: 0,
                    tool_counter: Arc::new(Mutex::new(0)),
                },
                true,
            )),
            Err(e) => Err(e),
        }
    });
    let (mut kernel, state_reset) = match acquired {
        Ok(pair) => pair,
        Err(e) => {
            return json!({
                "status": "error", "error": e,
                "tool_calls_made": 0, "duration_seconds": 0.0,
            })
            .to_string()
        }
    };
    let reused = !state_reset;

    let deadline = Instant::now() + Duration::from_secs(timeout);
    let cell_id = format!("{:032x}", fnv_digest(code.as_bytes()));
    if let Err(e) = kernel.child.write_cell(&cell_id, &code) {
        kernel.child.kill();
        return error_result(&e, 0, &exec_start).to_string();
    }
    kernel.execution_count += 1;
    *kernel.tool_counter.lock().unwrap() = 0;
    let tool_calls_made = *kernel.tool_counter.lock().unwrap();
    let reused_cell = reused;
    let duration = elapsed(exec_start);
    match kernel.child.read_reply(deadline) {
        Err(status) => {
            // No safe way to interrupt one cell in place: kill the kernel,
            // report the loss, respawn next call (Python `_cell_result`).
            kernel.child.kill();
            with_kernels(|map| {
                map.remove(owner);
            });
            let raw = kernel.child.take_raw();
            let stderr = kernel.child.take_stderr();
            let (stdout_text, stdout_metadata) = truncate_stdout_text(&format!("{raw}{stderr}"));
            let mut result = json!({
                "status": "error",
                "output": stdout_text,
                "exit_code": -1,
                "tool_calls_made": tool_calls_made,
                "duration_seconds": duration,
                "kernel": {
                    "mode": "session", "reused": reused_cell,
                    "execution_count": kernel.execution_count,
                    "state_reset": true,
                },
            });
            merge_metadata(&mut result, &stdout_metadata);
            if status == "timeout" {
                let message = format!(
                    "Cell timed out after {timeout}s; the session kernel was killed and its \
                     state was lost. The next execute_code call starts a fresh kernel."
                );
                result["error"] = json!(message);
                result["output"] = json!(format!(
                    "{}\n\n⏰ {message}",
                    result["output"].as_str().unwrap_or("")
                ));
            } else {
                result["error"] = json!("The session kernel died while running the cell.");
            }
            result.to_string()
        }
        Ok(payload) => {
            let cell_status = payload.get("status").and_then(Value::as_str).unwrap_or("");
            let raw = kernel.child.take_raw();
            let cell_stderr = payload.get("stderr").and_then(Value::as_str).unwrap_or("");
            let trace = payload
                .get("traceback")
                .and_then(Value::as_str)
                .unwrap_or("");
            let cell_stdout = payload.get("stdout").and_then(Value::as_str).unwrap_or("");
            // Python `_with_stderr`: stderr and the traceback ride in the
            // output under one marker so the model sees the failure inline.
            let combined = if cell_stderr.is_empty() && trace.is_empty() {
                format!("{cell_stdout}{raw}")
            } else {
                format!("{cell_stdout}\n--- stderr ---\n{cell_stderr}{trace}{raw}")
            };
            let (stdout_text, stdout_metadata) = truncate_stdout_text(&combined);
            let mut result = json!({
                "status": if cell_status == "ok" { "success" } else { "error" },
                "output": stdout_text,
                "exit_code": if cell_status == "ok" { 0 } else { 1 },
                "tool_calls_made": tool_calls_made,
                "duration_seconds": duration,
                "kernel": {
                    "mode": "session", "reused": reused_cell,
                    "execution_count": kernel.execution_count,
                    "state_reset": state_reset,
                },
            });
            merge_metadata(&mut result, &stdout_metadata);
            if cell_status == "error" {
                result["error"] = json!(if trace.is_empty() {
                    "Cell raised an exception."
                } else {
                    trace
                });
                if let Some(hint) = sandbox_failure_hint(trace) {
                    result["hint"] = json!(hint);
                }
            }
            if cell_status == "exit" {
                // The cell called sys.exit(): honor it as end-of-kernel.
                kernel.child.kill();
                with_kernels(|map| {
                    map.remove(owner);
                });
                result["kernel"]["ended"] = json!(true);
            } else if cell_status == "ok" {
                // Keep the kernel for the next cell of this owner.
                with_kernels(|map| {
                    map.insert(owner.to_string(), kernel);
                });
            }
            result.to_string()
        }
    }
}

fn merge_metadata(result: &mut Value, metadata: &Value) {
    if let (Some(map), Some(extra)) = (result.as_object_mut(), metadata.as_object()) {
        for (k, v) in extra {
            map.insert(k.clone(), v.clone());
        }
    }
}

fn with_kernels<R>(f: impl FnOnce(&mut std::collections::HashMap<String, LiveKernel>) -> R) -> R {
    let mut guard = KERNELS.lock().unwrap();
    let map = guard.get_or_insert_with(std::collections::HashMap::new);
    f(map)
}

fn new_staging(sandbox_tools: &[&str]) -> KernelStaging {
    let dir = std::env::temp_dir().join(format!(
        "hermes_kernel_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    KernelStaging {
        hermes_tools_py: generate_hermes_tools_module(sandbox_tools, "uds"),
        runner_py: KERNEL_RUNNER_SOURCE.to_string(),
        rpc_endpoint: "tcp://127.0.0.1:0".into(),
        rpc_token: random_token(),
        sentinel: format!("@@HERMES-KERNEL-{}@@", random_token()),
        dir,
    }
}

fn random_token() -> String {
    format!("{:032x}", fnv_digest(&std::process::id().to_le_bytes()))
}

fn elapsed(start: Instant) -> f64 {
    (start.elapsed().as_millis() as f64 / 1000.0 * 100.0).round() / 100.0
}

fn error_result(error: &str, tool_calls_made: u64, exec_start: &Instant) -> Value {
    json!({
        "status": "error",
        "error": error,
        "tool_calls_made": tool_calls_made,
        "duration_seconds": elapsed(*exec_start),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// Serialize the global-kernel state across tests.
    fn guard() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap()
    }

    // ---- stub module generation ----

    #[test]
    fn generated_module_lists_only_enabled_tools() {
        let module = generate_hermes_tools_module(&["read_file", "terminal"], "uds");
        assert!(
            module.contains("def read_file(path: str, offset: int = 1"),
            "{module}"
        );
        assert!(module.contains("def terminal(command: str"), "{module}");
        assert!(!module.contains("def web_search("), "{module}");
        assert!(module.contains("tcp://127.0.0.1"), "{module}");
        assert!(module.contains("def json_parse(text: str):"), "{module}");
        // Deterministic sorted order.
        let read = module.find("def read_file(").unwrap();
        let term = module.find("def terminal(").unwrap();
        assert!(read < term);
        let remote = generate_hermes_tools_module(&["read_file"], "file");
        assert!(remote.contains("HERMES_RPC_DIR"), "{remote}");
        assert!(!remote.contains("socket.socket(socket.AF_UNIX"), "{remote}");
    }

    #[test]
    fn empty_enabled_tools_generates_no_stubs() {
        let module = generate_hermes_tools_module(&[], "uds");
        assert!(!module.contains("def read_file("), "{module}");
        assert!(module.contains("def _call("), "{module}");
    }

    // ---- output pipeline ----

    #[test]
    fn stdout_within_the_cap_passes_through() {
        let (text, meta) = truncate_stdout_text("hello");
        assert_eq!(text, "hello");
        assert_eq!(meta["stdout_truncated"], false);
        assert_eq!(meta["stdout_bytes_total"], 5);
        assert!(meta.get("warning").is_none());
    }

    #[test]
    fn oversized_stdout_is_head_tailed_with_a_spill() {
        let long = "x".repeat(MAX_STDOUT_BYTES + 1000);
        let (text, meta) = truncate_stdout_text(&long);
        assert_eq!(meta["stdout_truncated"], true);
        assert_eq!(meta["stdout_bytes_captured"], MAX_STDOUT_BYTES);
        assert_eq!(meta["stdout_bytes_omitted"], 1000);
        assert!(text.contains("truncated 1000 of"), "{text}");
        let spill = meta["stdout_spill_path"].as_str().unwrap();
        assert!(std::path::Path::new(spill).is_file(), "{spill}");
        assert!(meta["warning"]
            .as_str()
            .unwrap()
            .contains("FULL output saved to"));
        let _ = std::fs::remove_file(spill);
    }

    // ---- failure hints ----

    #[test]
    fn hints_match_the_wellknown_failures() {
        let hint = sandbox_failure_hint(
            "Traceback ...\nImportError: cannot import name 'foo' from 'hermes_tools'",
        )
        .unwrap();
        assert!(
            hint.contains("'foo' is not available inside the execute_code sandbox"),
            "{hint}"
        );
        let hint = sandbox_failure_hint("NameError: name 'json_parse' is not defined").unwrap();
        assert!(
            hint.starts_with("Import json_parse before calling it"),
            "{hint}"
        );
        let hint = sandbox_failure_hint("ModuleNotFoundError: No module named 'pandas'").unwrap();
        assert!(
            hint.contains("'pandas' is not installed in the sandbox interpreter"),
            "{hint}"
        );
        let hint = sandbox_failure_hint("TypeError: string indices must be integers").unwrap();
        assert!(hint.contains("return DICTS"), "{hint}");
        assert!(sandbox_failure_hint("").is_none());
        assert!(sandbox_failure_hint("some unrelated error").is_none());
    }

    // ---- frame protocol parser ----

    #[test]
    fn frame_parser_splits_frames_from_raw_output() {
        // Mirrors read_reply: the marker carries the leading newline, so the
        // bytes before it stay buffered as the running cell's raw output.
        let mut buf = b"print noise before\n@@S@ 7\n{\"a\":1}".to_vec();
        let payload = parse_frame(&mut buf, "\n@@S@ ").unwrap();
        assert_eq!(payload["a"], 1);
        assert_eq!(String::from_utf8(buf).unwrap(), "print noise before");
    }

    // ---- tool RPC server ----

    fn spawn_rpc_pair(max: u64) -> (std::net::TcpStream, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::net::TcpStream::connect(addr).unwrap();
        // Placeholder for the unused half of the returned pair; connects after
        // `client`, so the thread's accept below claims `client` itself.
        let placeholder = std::net::TcpStream::connect(addr).unwrap();
        let counter = Arc::new(Mutex::new(0u64));
        let log = Arc::new(Mutex::new(vec![]));
        let stop = Arc::new(AtomicBool::new(false));
        let counter_thread = Arc::clone(&counter);
        let log_thread = Arc::clone(&log);
        let stop_thread = Arc::clone(&stop);
        std::thread::spawn(move || {
            let mut server = listener.accept().unwrap().0;
            rpc_server_loop(
                &mut server,
                &|tool, args| json!({ "status": "ok", "tool": tool, "args": args }).to_string(),
                "tok",
                SANDBOX_ALLOWED_TOOLS,
                max,
                &counter_thread,
                &log_thread,
                &stop_thread,
            );
        });
        (placeholder, client)
    }

    fn read_line(stream: &mut std::net::TcpStream) -> String {
        let mut buf = Vec::new();
        let mut byte = [0u8; 1];
        while buf.last() != Some(&b'\n') {
            match stream.read(&mut byte) {
                Ok(1) => buf.push(byte[0]),
                _ => break,
            }
        }
        String::from_utf8_lossy(&buf).to_string()
    }

    #[test]
    fn rpc_server_authenticates_whitelists_and_budgets() {
        let (_server, mut client) = spawn_rpc_pair(1);
        // Token mismatch.
        client
            .write_all(br#"{"tool": "read_file", "args": {}, "token": "wrong"}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        let reply = read_line(&mut client);
        assert!(reply.contains("token mismatch"), "{reply}");
        // Unknown tool.
        client
            .write_all(br#"{"tool": "browser_click", "args": {}, "token": "tok"}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        let reply = read_line(&mut client);
        assert!(
            reply.contains("not available inside the execute_code sandbox"),
            "{reply}"
        );
        // The one allowed call.
        client
            .write_all(br#"{"tool": "read_file", "args": {"path": "a"}, "token": "tok"}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        let reply = read_line(&mut client);
        assert!(reply.contains(r#""status":"ok""#), "{reply}");
        // Budget exhausted.
        client
            .write_all(br#"{"tool": "read_file", "args": {}, "token": "tok"}"#)
            .unwrap();
        client.write_all(b"\n").unwrap();
        let reply = read_line(&mut client);
        assert!(reply.contains("budget exhausted"), "{reply}");
    }

    // ---- end-to-end against a stub kernel ----

    struct StubChild {
        payload: Mutex<Option<Value>>,
        killed: std::sync::atomic::AtomicBool,
    }

    impl KernelChild for StubChild {
        fn write_cell(&mut self, _id: &str, _code: &str) -> Result<(), String> {
            Ok(())
        }
        fn read_reply(&mut self, _deadline: Instant) -> Result<Value, String> {
            self.payload
                .lock()
                .unwrap()
                .take()
                .ok_or_else(|| "kernel-eof".to_string())
        }
        fn take_raw(&mut self) -> String {
            String::new()
        }
        fn take_stderr(&mut self) -> String {
            String::new()
        }
        fn alive(&mut self) -> bool {
            !self.killed.load(Ordering::Relaxed)
        }
        fn kill(&mut self) {
            self.killed.store(true, Ordering::Relaxed);
        }
    }

    struct StubSpawner {
        counter: std::sync::atomic::AtomicUsize,
        payload: Mutex<Value>,
    }

    impl KernelSpawner for StubSpawner {
        fn spawn(&self, _staging: &KernelStaging) -> Result<Box<dyn KernelChild>, String> {
            self.counter.fetch_add(1, Ordering::Relaxed);
            let payload = self.payload.lock().unwrap().clone();
            Ok(Box::new(StubChild {
                payload: Mutex::new(Some(payload)),
                killed: std::sync::atomic::AtomicBool::new(false),
            }))
        }
    }

    #[test]
    fn stub_kernel_success_cell() {
        let _g = guard();
        let spawner = Arc::new(StubSpawner {
            counter: std::sync::atomic::AtomicUsize::new(0),
            payload: Mutex::new(json!({
                "id": "", "status": "ok", "stdout": "42", "stderr": "",
                "stdout_clipped": false, "stderr_clipped": false,
                "traceback": "", "execution_count": 1,
            })),
        });
        set_kernel_spawner(Some(spawner.clone()));
        let out = execute(&json!({ "code": "print(42)", "task_id": "t-stub-1" }));
        let result: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(result["status"], "success", "{out}");
        assert_eq!(result["output"], "42");
        assert_eq!(result["exit_code"], 0);
        assert_eq!(result["kernel"]["mode"], "session");
        assert_eq!(result["kernel"]["state_reset"], true);
        assert_eq!(spawner.counter.load(Ordering::Relaxed), 1);
        // Second call reuses the kernel (no respawn).
        let out = execute(&json!({ "code": "1", "task_id": "t-stub-1" }));
        assert!(out.contains(r#""reused":true"#), "{out}");
        assert_eq!(spawner.counter.load(Ordering::Relaxed), 1);
        // reset=true respawns.
        let out = execute(&json!({ "code": "1", "task_id": "t-stub-1", "reset": true }));
        assert!(out.contains(r#""state_reset":true"#), "{out}");
        assert_eq!(spawner.counter.load(Ordering::Relaxed), 2);
        set_kernel_spawner(None);
    }

    #[test]
    fn stub_kernel_error_cell_carries_the_hint() {
        let _g = guard();
        let spawner = Arc::new(StubSpawner {
            counter: std::sync::atomic::AtomicUsize::new(0),
            payload: Mutex::new(json!({
                "id": "", "status": "error", "stdout": "", "stderr": "",
                "stdout_clipped": false, "stderr_clipped": false,
                "traceback":
                    "Traceback (most recent call last):\nModuleNotFoundError: No module named 'pandas'",
                "execution_count": 1,
            })),
        });
        set_kernel_spawner(Some(spawner));
        let out = execute(&json!({ "code": "import pandas", "task_id": "t-stub-2" }));
        let result: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(result["status"], "error");
        assert!(
            result["error"].as_str().unwrap().contains("pandas"),
            "{out}"
        );
        assert!(
            result["hint"].as_str().unwrap().contains("not installed"),
            "{out}"
        );
        // A failed cell does NOT persist the kernel.
        let out = execute(&json!({ "code": "1", "task_id": "t-stub-2" }));
        assert!(out.contains(r#""reused":false"#), "{out}");
        set_kernel_spawner(None);
    }

    #[test]
    fn validation_matches_python() {
        let _g = guard();
        // 'command' instead of 'code'.
        let out = execute(&json!({ "command": "ls" }));
        assert!(out.contains("received a 'command' parameter"), "{out}");
        // Non-string code.
        let out = execute(&json!({ "code": 42 }));
        assert!(out.contains("received a int in 'code'"), "{out}");
        // Empty code.
        let out = execute(&json!({ "code": "   " }));
        assert!(out.contains("No code provided"), "{out}");
        let out = execute(&json!({}));
        assert!(out.contains("No code provided"), "{out}");
    }
}
