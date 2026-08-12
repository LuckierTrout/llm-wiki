//! Headless web server that exposes the complete Tauri command surface over
//! HTTP so the desktop app's frontend can run in an ordinary browser.
//!
//! Architecture:
//! - The app is built on `tauri::test::MockRuntime` — no display, no webview.
//!   Every `#[tauri::command]` function is called directly by the dispatcher
//!   below, with the same managed state the desktop app registers.
//! - `POST /api/invoke/{command}` mirrors Tauri's `invoke()` (JSON args in,
//!   JSON value or error string out).
//! - `GET /api/events` is a single SSE stream carrying every event the
//!   backend emits (`agent-event`, `file-sync://*`, `claude-cli:*`,
//!   `codex-cli:*`), replacing Tauri's event system in the browser.
//! - `GET /api/asset` replaces the `asset:` protocol (`convertFileSrc`).
//! - `POST /api/http-proxy` replaces `tauri-plugin-http` (Rust-side fetch
//!   that bypasses browser CORS for third-party LLM endpoints).
//! - `/api/store/{file}` persists `app-state.json` server-side — the same
//!   file `proxy.rs`, `api_server.rs` and `server_bind.rs` read on disk.
//! - `/api/v1/*` and `/clip/*` reverse-proxy to the embedded local HTTP API
//!   (port 19828) and web-clipper server (port 19827).
//! - Everything else serves the built frontend (SPA fallback to index.html).

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use base64::Engine;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use tauri::{Listener, Manager};
use tiny_http::{Header, Method, Response, Server, StatusCode};

use crate::commands;
use crate::proxy;
use crate::WikiAppHandle;

const DEFAULT_PORT: u16 = 8080;
const DEFAULT_BIND: &str = "0.0.0.0";
const MAX_INVOKE_BODY_BYTES: usize = 256 * 1024 * 1024;
const MAX_PROXY_BODY_BYTES: usize = 64 * 1024 * 1024;
const MAX_UPLOAD_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const MAX_IN_FLIGHT_REQUESTS: usize = 256;
const SSE_QUEUE_CAPACITY: usize = 256;
const SSE_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);
const SESSION_COOKIE: &str = "llmwiki_session";
const LOCAL_API_BASE: &str = "http://127.0.0.1:19828";
const LOCAL_CLIP_BASE: &str = "http://127.0.0.1:19827";

static IN_FLIGHT: AtomicUsize = AtomicUsize::new(0);

// ---------------------------------------------------------------------------
// SSE broadcast bus
// ---------------------------------------------------------------------------

static SSE_CLIENTS: OnceLock<Mutex<Vec<SyncSender<Vec<u8>>>>> = OnceLock::new();

fn sse_clients() -> &'static Mutex<Vec<SyncSender<Vec<u8>>>> {
    SSE_CLIENTS.get_or_init(|| Mutex::new(Vec::new()))
}

/// Forward one backend event to every connected SSE client. `payload_json`
/// is the JSON-serialized event payload exactly as Tauri would deliver it.
/// serde_json output never contains raw newlines, so a single `data:` line
/// is always well-formed.
fn sse_broadcast(event: &str, payload_json: &str) {
    let frame = format!("event: {event}\ndata: {payload_json}\n\n").into_bytes();
    if let Ok(mut clients) = sse_clients().lock() {
        clients.retain(|tx| match tx.try_send(frame.clone()) {
            Ok(()) => true,
            // A slow client may drop intermediate frames; terminal state is
            // recoverable via invoke commands, matching api_server.rs policy.
            Err(mpsc::TrySendError::Full(_)) => true,
            Err(mpsc::TrySendError::Disconnected(_)) => false,
        });
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run() {
    let port = std::env::var("LLM_WIKI_WEB_PORT")
        .or_else(|_| std::env::var("PORT"))
        .ok()
        .and_then(|value| value.parse::<u16>().ok())
        .unwrap_or(DEFAULT_PORT);
    let bind = std::env::var("LLM_WIKI_WEB_BIND").unwrap_or_else(|_| DEFAULT_BIND.to_string());

    let app = tauri::test::mock_builder()
        .build(tauri::generate_context!())
        .expect("failed to build headless app");
    let handle = app.handle().clone();

    // Mirror the desktop setup() hook: managed state, pdfium resource hint,
    // proxy env, embedded servers.
    handle.manage(commands::claude_cli::ClaudeCliState::default());
    handle.manage(commands::codex_cli::CodexCliState::default());
    handle.manage(commands::file_sync::FileSyncState::default());
    handle.manage(crate::agent::session::AgentSessionStore::default());
    handle.manage(crate::agent::cancel::AgentCancellationRegistry::default());
    handle.manage(crate::CloseBehaviorState(Mutex::new("minimize".to_string())));
    handle.manage(crate::TrayAvailabilityState(Mutex::new(false)));

    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            commands::fs::set_resource_dir_hint(dir.to_path_buf());
        }
    }

    match handle.path().app_data_dir() {
        Ok(dir) => {
            if let Err(err) = std::fs::create_dir_all(&dir) {
                eprintln!("[web] failed to create app data dir {}: {err}", dir.display());
            }
            let store_path = dir.join("app-state.json");
            eprintln!("[web] app data dir: {}", dir.display());
            if let Some(cfg) = proxy::read_proxy_config_from_store(&store_path) {
                let summary = proxy::apply_proxy_env(&cfg);
                eprintln!("[proxy] {summary}");
            }
        }
        Err(err) => eprintln!("[web] could not resolve app_data_dir: {err}"),
    }

    // Statically-named backend events, forwarded to the browser for the
    // lifetime of the process. CLI stream topics are dynamic and registered
    // per spawn in the dispatcher.
    for name in ["agent-event", "file-sync://queue-updated", "file-sync://changed"] {
        let event_name = name.to_string();
        handle.listen_any(name, move |event| {
            sse_broadcast(&event_name, event.payload());
        });
    }

    clip_server_bootstrap(&handle);
    crate::clip_server::start_clip_server(handle.clone());
    crate::api_server::start_api_server(handle.clone());

    let web_root = resolve_web_root();
    match &web_root {
        Some(root) => eprintln!("[web] serving frontend from {}", root.display()),
        None => eprintln!(
            "[web] WARNING: no frontend build found (set LLM_WIKI_WEB_ROOT or place `dist/` next to the binary)"
        ),
    }
    if required_token().is_none() {
        eprintln!(
            "[web] WARNING: LLM_WIKI_WEB_PASSWORD is not set — the server is unauthenticated. \
             Anyone who can reach this port has full read/write access to the wiki data and \
             outbound HTTP proxying. Set a password before exposing it beyond localhost."
        );
    }

    let addr = format!("{bind}:{port}");
    let server = match Server::http(&addr) {
        Ok(server) => server,
        Err(err) => {
            eprintln!("[web] failed to bind {addr}: {err}");
            std::process::exit(1);
        }
    };
    eprintln!("[web] LLM Wiki web server listening on http://{addr}");

    for request in server.incoming_requests() {
        if IN_FLIGHT.load(Ordering::Relaxed) >= MAX_IN_FLIGHT_REQUESTS {
            respond_json_value(request, 503, &json!({ "ok": false, "error": "Server is busy" }));
            continue;
        }
        IN_FLIGHT.fetch_add(1, Ordering::Relaxed);
        let handle = handle.clone();
        let web_root = web_root.clone();
        thread::spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                handle_request(request, &handle, web_root.as_deref());
            }));
            IN_FLIGHT.fetch_sub(1, Ordering::Relaxed);
            if result.is_err() {
                eprintln!("[web] request handler panicked");
            }
        });
    }

    // Keep the app alive for as long as the accept loop runs.
    drop(app);
}

/// Seed the clip server's project registry from app-state.json so `current`
/// project resolution works before the frontend connects (the desktop app
/// does this from the frontend on startup).
fn clip_server_bootstrap(app: &WikiAppHandle) {
    let Ok(dir) = app.path().app_data_dir() else {
        return;
    };
    let Ok(raw) = std::fs::read_to_string(dir.join("app-state.json")) else {
        return;
    };
    let Ok(parsed) = serde_json::from_str::<Value>(&raw) else {
        return;
    };
    if let Some(last) = parsed.get("lastProject").and_then(|p| p.get("path")).and_then(Value::as_str) {
        crate::clip_server::set_current_project_path(last);
    }
}

fn resolve_web_root() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(explicit) = std::env::var("LLM_WIKI_WEB_ROOT") {
        candidates.push(PathBuf::from(explicit));
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("dist"));
            candidates.push(dir.join("../dist"));
        }
    }
    if let Ok(cwd) = std::env::current_dir() {
        candidates.push(cwd.join("dist"));
        candidates.push(cwd.join("../dist"));
    }
    candidates
        .into_iter()
        .find(|dir| dir.join("index.html").is_file())
        .and_then(|dir| dir.canonicalize().ok())
}

// ---------------------------------------------------------------------------
// Auth
// ---------------------------------------------------------------------------

fn required_token() -> Option<String> {
    std::env::var("LLM_WIKI_WEB_PASSWORD")
        .or_else(|_| std::env::var("LLM_WIKI_WEB_TOKEN"))
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn header_value(request: &tiny_http::Request, name: &str) -> Option<String> {
    request
        .headers()
        .iter()
        .find(|header| header.field.as_str().as_str().eq_ignore_ascii_case(name))
        .map(|header| header.value.as_str().to_string())
}

fn cookie_value(request: &tiny_http::Request, name: &str) -> Option<String> {
    let cookies = header_value(request, "Cookie")?;
    for pair in cookies.split(';') {
        let mut parts = pair.trim().splitn(2, '=');
        let key = parts.next()?.trim();
        if key == name {
            return parts.next().map(|value| value.trim().to_string());
        }
    }
    None
}

fn presented_token(request: &tiny_http::Request, query: &str) -> Option<String> {
    if let Some(auth) = header_value(request, "Authorization") {
        if let Some(token) = auth.strip_prefix("Bearer ") {
            return Some(token.trim().to_string());
        }
    }
    if let Some(token) = header_value(request, "X-LLM-Wiki-Web-Token") {
        return Some(token.trim().to_string());
    }
    if let Some(token) = cookie_value(request, SESSION_COOKIE) {
        return Some(token);
    }
    query_param(query, "token")
}

fn is_authorized(request: &tiny_http::Request, query: &str) -> bool {
    match required_token() {
        None => true,
        Some(expected) => presented_token(request, query)
            .map(|token| constant_time_eq(&token, &expected))
            .unwrap_or(false),
    }
}

// ---------------------------------------------------------------------------
// Small HTTP helpers
// ---------------------------------------------------------------------------

fn query_param(query: &str, name: &str) -> Option<String> {
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        if parts.next() == Some(name) {
            return Some(crate::percent_decode(&parts.next().unwrap_or("").replace('+', " ")));
        }
    }
    None
}

fn json_header() -> Header {
    Header::from_bytes("Content-Type", "application/json; charset=utf-8").unwrap()
}

fn respond_json_value(request: tiny_http::Request, status: u16, value: &Value) {
    let body = value.to_string().into_bytes();
    let response = Response::from_data(body)
        .with_status_code(StatusCode(status))
        .with_header(json_header());
    let _ = request.respond(response);
}

fn respond_error(request: tiny_http::Request, status: u16, message: &str) {
    respond_json_value(request, status, &json!({ "ok": false, "error": message }));
}

fn read_body(request: &mut tiny_http::Request, limit: usize) -> Result<Vec<u8>, String> {
    let mut body = Vec::new();
    let mut reader = request.as_reader().take(limit as u64 + 1);
    reader
        .read_to_end(&mut body)
        .map_err(|err| format!("failed to read request body: {err}"))?;
    if body.len() > limit {
        return Err("request body too large".to_string());
    }
    Ok(body)
}

fn mime_for_path(path: &Path) -> &'static str {
    match path
        .extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("html") => "text/html; charset=utf-8",
        Some("js" | "mjs") => "text/javascript; charset=utf-8",
        Some("css") => "text/css; charset=utf-8",
        Some("json") => "application/json; charset=utf-8",
        Some("svg") => "image/svg+xml",
        Some("png") => "image/png",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("ico") => "image/x-icon",
        Some("bmp") => "image/bmp",
        Some("pdf") => "application/pdf",
        Some("txt" | "md" | "org") => "text/plain; charset=utf-8",
        Some("xml") => "application/xml",
        Some("wasm") => "application/wasm",
        Some("woff") => "font/woff",
        Some("woff2") => "font/woff2",
        Some("ttf") => "font/ttf",
        Some("otf") => "font/otf",
        Some("mp4" | "m4v") => "video/mp4",
        Some("webm") => "video/webm",
        Some("ogg" | "ogv") => "video/ogg",
        Some("mp3") => "audio/mpeg",
        Some("m4a") => "audio/mp4",
        Some("wav") => "audio/wav",
        Some("flac") => "audio/flac",
        Some("zip") => "application/zip",
        Some("epub") => "application/epub+zip",
        _ => "application/octet-stream",
    }
}

// ---------------------------------------------------------------------------
// Request routing
// ---------------------------------------------------------------------------

fn handle_request(request: tiny_http::Request, app: &WikiAppHandle, web_root: Option<&Path>) {
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path.to_string(), query.to_string()),
        None => (url.clone(), String::new()),
    };
    let method = request.method().clone();

    if method == Method::Options {
        let _ = request.respond(Response::empty(StatusCode(204)));
        return;
    }

    // Unauthenticated endpoints.
    if path == "/api/health" {
        respond_json_value(
            request,
            200,
            &json!({
                "status": "ok",
                "version": env!("CARGO_PKG_VERSION"),
                "authRequired": required_token().is_some(),
            }),
        );
        return;
    }
    if path == "/api/login" && method == Method::Post {
        respond_login(request);
        return;
    }

    let is_api = path.starts_with("/api/") || path.starts_with("/clip/");
    if is_api && !is_authorized(&request, &query) {
        respond_error(request, 401, "Unauthorized");
        return;
    }

    if let Some(command) = path.strip_prefix("/api/invoke/") {
        if method != Method::Post {
            respond_error(request, 405, "Method not allowed");
            return;
        }
        let command = command.to_string();
        respond_invoke(request, app, &command);
        return;
    }

    match (path.as_str(), &method) {
        ("/api/events", &Method::Get) => respond_events(request),
        ("/api/asset", &Method::Get) | ("/api/asset", &Method::Head) => {
            respond_fs_file(request, &query, false)
        }
        ("/api/download", &Method::Get) => respond_fs_file(request, &query, true),
        ("/api/upload", &Method::Post) => respond_upload(request, app, &query),
        ("/api/browse", &Method::Get) => respond_browse(request, app, &query),
        ("/api/http-proxy", &Method::Post) => respond_http_proxy(request),
        _ => {
            if path.starts_with("/api/store/") {
                respond_store(request, app, &path, &method);
            } else if path == "/api/v1" || path.starts_with("/api/v1/") {
                respond_proxy_pass(request, LOCAL_API_BASE, &url);
            } else if let Some(rest) = path.strip_prefix("/clip") {
                let rest = if rest.is_empty() { "/" } else { rest };
                let target = match query.is_empty() {
                    true => rest.to_string(),
                    false => format!("{rest}?{query}"),
                };
                respond_proxy_pass(request, LOCAL_CLIP_BASE, &target);
            } else if method == Method::Get || method == Method::Head {
                respond_static(request, web_root, &path);
            } else {
                respond_error(request, 404, "Not found");
            }
        }
    }
}

fn respond_login(mut request: tiny_http::Request) {
    let Some(expected) = required_token() else {
        respond_json_value(request, 200, &json!({ "ok": true, "authRequired": false }));
        return;
    };
    let body = match read_body(&mut request, 64 * 1024) {
        Ok(body) => body,
        Err(err) => {
            respond_error(request, 400, &err);
            return;
        }
    };
    let parsed: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    let password = parsed
        .get("password")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    if !constant_time_eq(&password, &expected) {
        respond_error(request, 401, "Invalid password");
        return;
    }
    let cookie = format!(
        "{SESSION_COOKIE}={password}; Path=/; HttpOnly; SameSite=Lax; Max-Age=2592000"
    );
    let response = Response::from_data(json!({ "ok": true }).to_string().into_bytes())
        .with_status_code(StatusCode(200))
        .with_header(json_header())
        .with_header(Header::from_bytes("Set-Cookie", cookie).unwrap());
    let _ = request.respond(response);
}

// ---------------------------------------------------------------------------
// SSE events endpoint
// ---------------------------------------------------------------------------

/// Write an HTTP/1.1 response head directly to the socket. tiny_http's
/// `respond()` sits behind a 1 KiB BufWriter that only flushes when full or
/// when the response body ends, which breaks unbounded streams (SSE, LLM
/// token streams) — so streaming responses bypass it via `into_writer()`
/// and flush after every frame. `Connection: close` + EOF-delimited body.
fn write_stream_head(
    writer: &mut dyn std::io::Write,
    status: u16,
    headers: &[(String, String)],
) -> std::io::Result<()> {
    let reason = match status {
        200 => "OK",
        401 => "Unauthorized",
        404 => "Not Found",
        _ => "Status",
    };
    write!(writer, "HTTP/1.1 {status} {reason}\r\n")?;
    for (name, value) in headers {
        write!(writer, "{name}: {value}\r\n")?;
    }
    writer.write_all(b"Connection: close\r\n\r\n")?;
    writer.flush()
}

fn respond_events(request: tiny_http::Request) {
    let (sender, receiver) = mpsc::sync_channel::<Vec<u8>>(SSE_QUEUE_CAPACITY);
    if let Ok(mut clients) = sse_clients().lock() {
        clients.push(sender);
    }
    let mut writer = request.into_writer();
    let head = [
        ("Content-Type".to_string(), "text/event-stream; charset=utf-8".to_string()),
        ("Cache-Control".to_string(), "no-cache, no-transform".to_string()),
        ("X-Accel-Buffering".to_string(), "no".to_string()),
    ];
    if write_stream_head(writer.as_mut(), 200, &head).is_err() {
        return;
    }
    if writer
        .write_all(b"retry: 3000\n\n")
        .and_then(|_| writer.flush())
        .is_err()
    {
        return;
    }
    // The request thread stays parked here for the lifetime of the stream;
    // keepalives double as disconnect detection. When a write fails, the
    // receiver drops and the broadcast bus prunes the matching sender.
    loop {
        let frame = match receiver.recv_timeout(SSE_HEARTBEAT_INTERVAL) {
            Ok(frame) => frame,
            Err(mpsc::RecvTimeoutError::Timeout) => b": keepalive\n\n".to_vec(),
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
        };
        if writer
            .write_all(&frame)
            .and_then(|_| writer.flush())
            .is_err()
        {
            break;
        }
    }
}

// ---------------------------------------------------------------------------
// File serving (asset protocol replacement) and uploads
// ---------------------------------------------------------------------------

fn respond_fs_file(request: tiny_http::Request, query: &str, download: bool) {
    let Some(path) = query_param(query, "path") else {
        respond_error(request, 400, "Missing `path` query parameter");
        return;
    };
    let path = PathBuf::from(path);
    if !path.is_absolute() {
        respond_error(request, 400, "Path must be absolute");
        return;
    }
    let Ok(file) = std::fs::File::open(&path) else {
        respond_error(request, 404, "File not found");
        return;
    };
    let Ok(metadata) = file.metadata() else {
        respond_error(request, 404, "File not found");
        return;
    };
    if !metadata.is_file() {
        respond_error(request, 400, "Not a file");
        return;
    }
    let mut response = Response::new(
        StatusCode(200),
        vec![
            Header::from_bytes("Content-Type", mime_for_path(&path)).unwrap(),
            Header::from_bytes("Cache-Control", "no-cache").unwrap(),
        ],
        file,
        Some(metadata.len() as usize),
        None,
    );
    if download {
        let name = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("download")
            .replace(['"', '\\', '\r', '\n'], "_");
        response.add_header(
            Header::from_bytes(
                "Content-Disposition",
                format!("attachment; filename=\"{name}\""),
            )
            .unwrap(),
        );
    }
    let _ = request.respond(response);
}

fn sanitize_file_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' | '\0' => '_',
            other => other,
        })
        .collect();
    let trimmed = cleaned.trim().trim_matches('.');
    if trimmed.is_empty() {
        "file".to_string()
    } else {
        trimmed.to_string()
    }
}

fn respond_upload(mut request: tiny_http::Request, app: &WikiAppHandle, query: &str) {
    let name = sanitize_file_name(&query_param(query, "name").unwrap_or_else(|| "file".to_string()));
    let Ok(data_dir) = app.path().app_data_dir() else {
        respond_error(request, 500, "App data dir unavailable");
        return;
    };
    let dir = data_dir.join("uploads").join(uuid::Uuid::new_v4().to_string());
    if let Err(err) = std::fs::create_dir_all(&dir) {
        respond_error(request, 500, &format!("failed to create upload dir: {err}"));
        return;
    }
    let dest = dir.join(&name);
    let mut file = match std::fs::File::create(&dest) {
        Ok(file) => file,
        Err(err) => {
            respond_error(request, 500, &format!("failed to create file: {err}"));
            return;
        }
    };
    let mut reader = request.as_reader().take(MAX_UPLOAD_BYTES + 1);
    let copied = match std::io::copy(&mut reader, &mut file) {
        Ok(copied) => copied,
        Err(err) => {
            let _ = std::fs::remove_file(&dest);
            respond_error(request, 500, &format!("upload failed: {err}"));
            return;
        }
    };
    if copied > MAX_UPLOAD_BYTES {
        let _ = std::fs::remove_file(&dest);
        respond_error(request, 413, "Upload too large");
        return;
    }
    respond_json_value(
        request,
        200,
        &json!({ "ok": true, "path": dest.to_string_lossy(), "size": copied }),
    );
}

/// Directory listing for the browser-side folder picker (dialog shim).
fn respond_browse(request: tiny_http::Request, app: &WikiAppHandle, query: &str) {
    let raw = query_param(query, "path").unwrap_or_default();
    let path = if raw.trim().is_empty() {
        default_browse_root(app)
    } else {
        PathBuf::from(raw)
    };
    let canonical = path.canonicalize().unwrap_or(path);
    let mut dirs: Vec<String> = Vec::new();
    match std::fs::read_dir(&canonical) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                if !file_type.is_dir() {
                    continue;
                }
                if let Some(name) = entry.file_name().to_str() {
                    if !name.starts_with('.') {
                        dirs.push(name.to_string());
                    }
                }
            }
        }
        Err(err) => {
            respond_error(request, 400, &format!("cannot list directory: {err}"));
            return;
        }
    }
    dirs.sort_by_key(|name| name.to_lowercase());
    let parent = canonical
        .parent()
        .map(|parent| parent.to_string_lossy().into_owned());
    respond_json_value(
        request,
        200,
        &json!({
            "ok": true,
            "path": canonical.to_string_lossy(),
            "parent": parent,
            "dirs": dirs,
            "sep": std::path::MAIN_SEPARATOR.to_string(),
        }),
    );
}

fn default_browse_root(app: &WikiAppHandle) -> PathBuf {
    if let Ok(dir) = std::env::var("LLM_WIKI_PROJECTS_DIR") {
        let path = PathBuf::from(dir);
        let _ = std::fs::create_dir_all(&path);
        return path;
    }
    if let Ok(dir) = app.path().app_data_dir() {
        let path = dir.join("projects");
        let _ = std::fs::create_dir_all(&path);
        return path;
    }
    PathBuf::from("/")
}

// ---------------------------------------------------------------------------
// Store endpoints (tauri-plugin-store replacement)
// ---------------------------------------------------------------------------

fn respond_store(mut request: tiny_http::Request, app: &WikiAppHandle, path: &str, method: &Method) {
    let name = path.trim_start_matches("/api/store/");
    if name.is_empty()
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    {
        respond_error(request, 400, "Invalid store name");
        return;
    }
    let Ok(data_dir) = app.path().app_data_dir() else {
        respond_error(request, 500, "App data dir unavailable");
        return;
    };
    let store_path = data_dir.join(name);
    match method {
        Method::Get => {
            let contents = std::fs::read_to_string(&store_path).unwrap_or_else(|_| "{}".to_string());
            let response = Response::from_data(contents.into_bytes())
                .with_status_code(StatusCode(200))
                .with_header(json_header())
                .with_header(Header::from_bytes("Cache-Control", "no-store").unwrap());
            let _ = request.respond(response);
        }
        Method::Post | Method::Put => {
            let body = match read_body(&mut request, 32 * 1024 * 1024) {
                Ok(body) => body,
                Err(err) => {
                    respond_error(request, 400, &err);
                    return;
                }
            };
            let parsed: Value = match serde_json::from_slice(&body) {
                Ok(parsed @ Value::Object(_)) => parsed,
                Ok(_) => {
                    respond_error(request, 400, "Store contents must be a JSON object");
                    return;
                }
                Err(err) => {
                    respond_error(request, 400, &format!("invalid JSON: {err}"));
                    return;
                }
            };
            if let Err(err) = std::fs::create_dir_all(&data_dir) {
                respond_error(request, 500, &format!("failed to create data dir: {err}"));
                return;
            }
            let tmp = store_path.with_extension("json.tmp");
            let serialized = serde_json::to_string_pretty(&parsed).unwrap_or_else(|_| "{}".into());
            if let Err(err) =
                std::fs::write(&tmp, serialized).and_then(|_| std::fs::rename(&tmp, &store_path))
            {
                respond_error(request, 500, &format!("failed to write store: {err}"));
                return;
            }
            if name == "app-state.json" {
                // api_server caches apiConfig from this file; the desktop app
                // relies on an explicit reload command, the web store keeps
                // it fresh automatically.
                crate::api_server::invalidate_config_cache();
                if let Some(last) = parsed
                    .get("lastProject")
                    .and_then(|p| p.get("path"))
                    .and_then(Value::as_str)
                {
                    crate::clip_server::set_current_project_path(last);
                }
            }
            respond_json_value(request, 200, &json!({ "ok": true }));
        }
        _ => respond_error(request, 405, "Method not allowed"),
    }
}

// ---------------------------------------------------------------------------
// Outbound HTTP proxy (tauri-plugin-http replacement)
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct ProxyFetchRequest {
    url: String,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    headers: Vec<(String, String)>,
    #[serde(default)]
    body_b64: Option<String>,
}

fn respond_http_proxy(mut request: tiny_http::Request) {
    let body = match read_body(&mut request, MAX_PROXY_BODY_BYTES) {
        Ok(body) => body,
        Err(err) => {
            respond_error(request, 400, &err);
            return;
        }
    };
    let envelope: ProxyFetchRequest = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(err) => {
            respond_error(request, 400, &format!("invalid proxy request: {err}"));
            return;
        }
    };
    if !envelope.url.starts_with("http://") && !envelope.url.starts_with("https://") {
        respond_error(request, 400, "Only http(s) URLs are supported");
        return;
    }
    let method = envelope.method.as_deref().unwrap_or("GET").to_uppercase();
    let Ok(method) = reqwest::Method::from_bytes(method.as_bytes()) else {
        respond_error(request, 400, "Invalid method");
        return;
    };
    let request_body = match envelope.body_b64.as_deref() {
        None => None,
        Some(encoded) => match base64::engine::general_purpose::STANDARD.decode(encoded) {
            Ok(decoded) => Some(decoded),
            Err(err) => {
                respond_error(request, 400, &format!("invalid body encoding: {err}"));
                return;
            }
        },
    };

    let result = tauri::async_runtime::block_on(async move {
        let client = proxy::configure_http_client(reqwest::Client::builder())
            .build()
            .map_err(|err| err.to_string())?;
        let mut outbound = client.request(method, &envelope.url);
        for (name, value) in &envelope.headers {
            let lowered = name.to_ascii_lowercase();
            if matches!(lowered.as_str(), "host" | "content-length" | "connection") {
                continue;
            }
            outbound = outbound.header(name, value);
        }
        if let Some(body) = request_body {
            outbound = outbound.body(body);
        }
        outbound.send().await.map_err(|err| err.to_string())
    });

    match result {
        Ok(upstream) => stream_upstream_response(request, upstream, true),
        Err(err) => respond_json_value(
            request,
            502,
            &json!({ "ok": false, "error": err, "proxyError": true }),
        ),
    }
}

/// Send a reqwest response back through tiny_http, streaming the body.
/// When `envelope_headers` is set, the upstream status/headers are exposed
/// via `X-Proxy-Status` / `X-Proxy-Headers` (base64 JSON) so the fetch shim
/// can reconstruct a faithful `Response` object even for error statuses.
fn stream_upstream_response(
    request: tiny_http::Request,
    upstream: reqwest::Response,
    envelope_headers: bool,
) {
    let status = upstream.status().as_u16();
    let mut header_list: Vec<(String, String)> = Vec::new();
    for (name, value) in upstream.headers() {
        if let Ok(value) = value.to_str() {
            header_list.push((name.as_str().to_string(), value.to_string()));
        }
    }

    let mut headers: Vec<(String, String)> = Vec::new();
    if envelope_headers {
        let encoded = base64::engine::general_purpose::STANDARD
            .encode(serde_json::to_string(&header_list).unwrap_or_else(|_| "[]".into()));
        headers.push(("X-Proxy-Status".into(), status.to_string()));
        headers.push(("X-Proxy-Headers".into(), encoded));
        if let Some((_, content_type)) = header_list
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-type"))
        {
            headers.push(("Content-Type".into(), content_type.clone()));
        }
        headers.push(("Cache-Control".into(), "no-store".into()));
    } else {
        for (name, value) in &header_list {
            let lowered = name.to_ascii_lowercase();
            if matches!(
                lowered.as_str(),
                "transfer-encoding" | "connection" | "content-length" | "keep-alive"
            ) || value.contains(['\r', '\n'])
            {
                continue;
            }
            headers.push((name.clone(), value.clone()));
        }
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
    tauri::async_runtime::spawn(async move {
        let mut upstream = upstream;
        loop {
            match upstream.chunk().await {
                Ok(Some(chunk)) => {
                    if tx.send(chunk.to_vec()).await.is_err() {
                        break;
                    }
                }
                Ok(None) | Err(_) => break,
            }
        }
    });

    // Stream through the raw socket, flushing per chunk, so token-by-token
    // LLM responses and proxied SSE reach the browser as they arrive
    // instead of stalling in tiny_http's 1 KiB write buffer.
    let response_status = if envelope_headers { 200 } else { status };
    let mut writer = request.into_writer();
    if write_stream_head(writer.as_mut(), response_status, &headers).is_err() {
        return;
    }
    while let Some(chunk) = rx.blocking_recv() {
        if writer
            .write_all(&chunk)
            .and_then(|_| writer.flush())
            .is_err()
        {
            break;
        }
    }
}

/// Reverse proxy for the embedded local servers (API :19828, clipper :19827).
fn respond_proxy_pass(mut request: tiny_http::Request, base: &str, path_and_query: &str) {
    let body = match read_body(&mut request, MAX_PROXY_BODY_BYTES) {
        Ok(body) => body,
        Err(err) => {
            respond_error(request, 400, &err);
            return;
        }
    };
    let Ok(method) = reqwest::Method::from_bytes(request.method().as_str().as_bytes()) else {
        respond_error(request, 400, "Invalid method");
        return;
    };
    let url = format!("{base}{path_and_query}");
    let mut forwarded_headers: Vec<(String, String)> = Vec::new();
    for header in request.headers() {
        let name = header.field.as_str().as_str().to_string();
        let lowered = name.to_ascii_lowercase();
        if matches!(
            lowered.as_str(),
            "host" | "content-length" | "connection" | "cookie" | "accept-encoding"
        ) {
            continue;
        }
        forwarded_headers.push((name, header.value.as_str().to_string()));
    }

    let result = tauri::async_runtime::block_on(async move {
        let client = reqwest::Client::builder()
            .no_proxy()
            .build()
            .map_err(|err| err.to_string())?;
        let mut outbound = client.request(method, &url);
        for (name, value) in &forwarded_headers {
            outbound = outbound.header(name, value);
        }
        if !body.is_empty() {
            outbound = outbound.body(body);
        }
        outbound.send().await.map_err(|err| err.to_string())
    });

    match result {
        Ok(upstream) => stream_upstream_response(request, upstream, false),
        Err(err) => respond_json_value(
            request,
            502,
            &json!({ "ok": false, "error": format!("local server unavailable: {err}") }),
        ),
    }
}

// ---------------------------------------------------------------------------
// Static frontend
// ---------------------------------------------------------------------------

fn respond_static(request: tiny_http::Request, web_root: Option<&Path>, path: &str) {
    let Some(root) = web_root else {
        respond_error(
            request,
            404,
            "Frontend build not found. Build it with `npm run build:web` and set LLM_WIKI_WEB_ROOT.",
        );
        return;
    };
    let decoded = crate::percent_decode(path);
    let relative = decoded.trim_start_matches('/');
    if relative.split('/').any(|segment| segment == "..") {
        respond_error(request, 400, "Invalid path");
        return;
    }
    let mut target = root.join(relative);
    if relative.is_empty() || !target.is_file() {
        // SPA fallback: any extension-less route serves the app shell.
        let last = relative.rsplit('/').next().unwrap_or("");
        if last.contains('.') && !relative.is_empty() {
            respond_error(request, 404, "Not found");
            return;
        }
        target = root.join("index.html");
    }
    let Ok(file) = std::fs::File::open(&target) else {
        respond_error(request, 404, "Not found");
        return;
    };
    let Ok(metadata) = file.metadata() else {
        respond_error(request, 404, "Not found");
        return;
    };
    let cache_control = if target.ends_with("index.html") || relative.is_empty() {
        "no-cache"
    } else if relative.starts_with("assets/") {
        "public, max-age=31536000, immutable"
    } else {
        "public, max-age=3600"
    };
    let response = Response::new(
        StatusCode(200),
        vec![
            Header::from_bytes("Content-Type", mime_for_path(&target)).unwrap(),
            Header::from_bytes("Cache-Control", cache_control).unwrap(),
        ],
        file,
        Some(metadata.len() as usize),
        None,
    );
    let _ = request.respond(response);
}

// ---------------------------------------------------------------------------
// Invoke dispatcher
// ---------------------------------------------------------------------------

fn respond_invoke(mut request: tiny_http::Request, app: &WikiAppHandle, command: &str) {
    let body = match read_body(&mut request, MAX_INVOKE_BODY_BYTES) {
        Ok(body) => body,
        Err(err) => {
            respond_error(request, 413, &err);
            return;
        }
    };
    let args: Map<String, Value> = if body.is_empty() {
        Map::new()
    } else {
        match serde_json::from_slice::<Value>(&body) {
            Ok(Value::Object(map)) => map,
            Ok(Value::Null) => Map::new(),
            Ok(_) => {
                respond_error(request, 400, "Arguments must be a JSON object");
                return;
            }
            Err(err) => {
                respond_error(request, 400, &format!("invalid JSON body: {err}"));
                return;
            }
        }
    };

    let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        dispatch(app, command, args)
    }));
    match outcome {
        Ok(Ok(value)) => respond_json_value(request, 200, &json!({ "ok": true, "value": value })),
        Ok(Err(DispatchError::Unknown)) => {
            respond_error(request, 404, &format!("Unknown command: {command}"))
        }
        Ok(Err(DispatchError::Command(message))) => {
            respond_json_value(request, 400, &json!({ "ok": false, "error": message }))
        }
        Err(_) => respond_error(request, 500, "Command handler panicked"),
    }
}

enum DispatchError {
    Unknown,
    Command(String),
}

impl From<String> for DispatchError {
    fn from(message: String) -> Self {
        DispatchError::Command(message)
    }
}

fn snake_to_camel(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut upper_next = false;
    for c in name.chars() {
        if c == '_' {
            upper_next = true;
        } else if upper_next {
            out.extend(c.to_uppercase());
            upper_next = false;
        } else {
            out.push(c);
        }
    }
    out
}

/// Pull one command argument out of the parsed JSON body. Accepts both the
/// Rust snake_case parameter name and Tauri's camelCase JS convention, like
/// the desktop IPC layer effectively does for this codebase.
fn take_arg<T: serde::de::DeserializeOwned>(
    args: &mut Map<String, Value>,
    name: &str,
) -> Result<T, String> {
    let value = args
        .remove(name)
        .or_else(|| args.remove(&snake_to_camel(name)))
        .unwrap_or(Value::Null);
    serde_json::from_value(value).map_err(|err| format!("invalid argument `{name}`: {err}"))
}

fn peek_string(args: &Map<String, Value>, name: &str) -> Result<String, String> {
    args.get(name)
        .or_else(|| args.get(&snake_to_camel(name)))
        .and_then(Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("missing argument `{name}`"))
}

fn json_result<T: serde::Serialize>(result: Result<T, String>) -> Result<Value, DispatchError> {
    match result {
        Ok(value) => serde_json::to_value(value)
            .map_err(|err| DispatchError::Command(format!("failed to serialize result: {err}"))),
        Err(message) => Err(DispatchError::Command(message)),
    }
}

fn json_plain<T: serde::Serialize>(value: T) -> Result<Value, DispatchError> {
    serde_json::to_value(value)
        .map_err(|err| DispatchError::Command(format!("failed to serialize result: {err}")))
}

/// Forward the per-stream CLI events (`{prefix}:{id}` and `{prefix}:{id}:done`)
/// to the SSE bus, unregistering both listeners once the stream finishes.
fn register_cli_stream_forwarders(app: &WikiAppHandle, prefix: &str, stream_id: &str) {
    let topic = format!("{prefix}:{stream_id}");
    let done_topic = format!("{topic}:done");
    let ids: Arc<Mutex<Vec<tauri::EventId>>> = Arc::new(Mutex::new(Vec::new()));

    let forward_topic = topic.clone();
    let id = app.listen_any(topic, move |event| {
        sse_broadcast(&forward_topic, event.payload());
    });
    ids.lock().map(|mut list| list.push(id)).ok();

    let forward_done = done_topic.clone();
    let unlisten_app = app.clone();
    let unlisten_ids = ids.clone();
    let done_id = app.listen_any(done_topic, move |event| {
        sse_broadcast(&forward_done, event.payload());
        let app = unlisten_app.clone();
        let ids = unlisten_ids.clone();
        // Unlisten from a fresh thread — removing listeners from inside a
        // handler would touch the listener registry re-entrantly.
        thread::spawn(move || {
            if let Ok(list) = ids.lock() {
                for id in list.iter() {
                    app.unlisten(*id);
                }
            }
        });
    });
    ids.lock().map(|mut list| list.push(done_id)).ok();
}

fn dispatch(
    app: &WikiAppHandle,
    command: &str,
    mut args: Map<String, Value>,
) -> Result<Value, DispatchError> {
    use crate::commands::{
        claude_cli, codex_cli, external_search, extract_images, file_history, file_sync, fs,
        project, project_maintenance, search, vectorstore,
    };
    use tauri::async_runtime::block_on;

    // Call an async command: `acall!(fn, arg_a, arg_b)` — argument idents are
    // the Rust parameter names; values come from the JSON body.
    macro_rules! acall {
        ($f:path $(, $p:ident)* $(,)?) => {
            json_result(block_on($f($(take_arg(&mut args, stringify!($p))?),*)))
        };
    }
    // Same, with `app` (and optionally managed state) as leading arguments.
    macro_rules! acall_app {
        ($f:path $(, $p:ident)* $(,)?) => {
            json_result(block_on($f(app.clone() $(, take_arg(&mut args, stringify!($p))?)*)))
        };
    }
    macro_rules! acall_app_state {
        ($f:path $(, $p:ident)* $(,)?) => {
            json_result(block_on($f(
                app.clone(),
                app.state(),
                $(take_arg(&mut args, stringify!($p))?),*
            )))
        };
    }
    macro_rules! acall_state {
        ($f:path $(, $p:ident)* $(,)?) => {
            json_result(block_on($f(app.state(), $(take_arg(&mut args, stringify!($p))?),*)))
        };
    }
    // Synchronous commands.
    macro_rules! scall {
        ($f:path $(, $p:ident)* $(,)?) => {
            json_result($f($(take_arg(&mut args, stringify!($p))?),*))
        };
    }
    macro_rules! scall_app {
        ($f:path $(, $p:ident)* $(,)?) => {
            json_result($f(app.clone(), $(take_arg(&mut args, stringify!($p))?),*))
        };
    }

    match command {
        // ---- fs ----
        "read_file" => acall!(fs::read_file, path, extract_images),
        "preprocess_file" => acall!(fs::preprocess_file, path),
        "write_file" => acall!(fs::write_file, path, contents),
        "write_file_base64" => acall!(fs::write_file_base64, path, base64),
        "write_file_atomic" => acall!(fs::write_file_atomic, path, contents),
        "apply_text_selection_edit" => acall!(
            fs::apply_text_selection_edit,
            project_path,
            file_path,
            prefix,
            selected_text,
            suffix,
            replacement,
        ),
        "create_missing_wiki_page" => {
            acall!(fs::create_missing_wiki_page, project_path, title, content)
        }
        "list_directory" => acall!(fs::list_directory, path, include_hidden, max_depth),
        "copy_file" => acall!(fs::copy_file, source, destination),
        "copy_directory" => acall!(fs::copy_directory, source, destination),
        "delete_file" => acall!(fs::delete_file, path),
        "find_related_wiki_pages" => acall!(fs::find_related_wiki_pages, project_path, source_name),
        "create_directory" => acall!(fs::create_directory, path),
        "read_file_as_base64" => acall!(fs::read_file_as_base64, path),
        "file_exists" => acall!(fs::file_exists, path),
        "get_file_modified_time" => acall!(fs::get_file_modified_time, path),
        "get_file_size" => acall!(fs::get_file_size, path),
        "get_file_md5" => acall!(fs::get_file_md5, path),

        // ---- file history ----
        "get_file_history_settings" => acall!(file_history::get_file_history_settings, project_path),
        "set_file_history_settings" => {
            acall!(file_history::set_file_history_settings, project_path, settings)
        }
        "list_file_history" => acall!(file_history::list_file_history, project_path, file_path),
        "restore_file_history" => acall!(
            file_history::restore_file_history,
            project_path,
            file_path,
            entry_id,
        ),
        "get_file_history_stats" => acall!(file_history::get_file_history_stats, project_path),
        "clear_file_history" => acall!(file_history::clear_file_history, project_path),

        // ---- project ----
        "create_project" => scall!(project::create_project, name, path),
        "open_project" => scall!(project::open_project, path),
        "open_project_folder" | "open_path_in_project" => Err(DispatchError::Command(
            "Opening paths in the system file manager is not available in the web version".into(),
        )),

        // ---- project maintenance ----
        "export_project_archive" => acall!(
            project_maintenance::export_project_archive,
            project_path,
            destination,
        ),
        "import_project_archive" => acall!(
            project_maintenance::import_project_archive,
            archive_path,
            destination,
        ),
        "rebuild_wiki_index" => acall!(project_maintenance::rebuild_wiki_index, project_path),

        // ---- search ----
        "search_project" => acall!(
            search::search_project,
            project_path,
            query,
            top_k,
            include_content,
            query_embedding,
            embedding_config,
        ),
        "embedding_fetch" => acall!(search::embedding_fetch, text, cfg, max_retries),
        "embedding_fetch_batch" => acall!(search::embedding_fetch_batch, texts, cfg),
        "get_page_links" => acall!(search::get_page_links, project_path, file_path),
        "web_search" => acall!(external_search::web_search, query, config, max_results),
        "anytxt_search" => acall!(external_search::anytxt_search, query, config, max_results),

        // ---- servers / app plumbing ----
        "clip_server_status" => json_plain(crate::clip_server_status()),
        "api_server_status" => json_plain(crate::api_server_status()),
        "api_server_reload_config" => json_plain(crate::api_server_reload_config()),
        "mcp_server_entry_path" => json_result(crate::mcp_server_entry_path(app.clone())),
        "set_proxy_env" => {
            let config: proxy::ProxyConfig = take_arg(&mut args, "config")?;
            json_plain(crate::set_proxy_env(config))
        }
        "set_close_behavior" => {
            let value: String = take_arg(&mut args, "value")?;
            json_result(crate::set_close_behavior(value, app.state()))
        }

        // ---- agent ----
        "agent_start_turn" => acall_app!(crate::agent_start_turn, project_id, request),
        "agent_start_turn_stream" => {
            acall_app!(crate::agent_start_turn_stream, project_id, request)
        }
        "agent_cancel_turn" => {
            scall_app!(crate::agent_cancel_turn, project_id, session_id, run_id)
        }
        "agent_get_session" => {
            scall_app!(crate::agent_get_session, project_id, session_id, limit)
        }
        "agent_list_sessions" => scall_app!(crate::agent_list_sessions, project_id),
        "agent_list_skills" => {
            let project_path: String = take_arg(&mut args, "project_path")?;
            json_plain(crate::agent::skills::agent_list_skills(project_path))
        }

        // ---- vector store ----
        "vector_upsert" => acall!(vectorstore::vector_upsert, project_path, page_id, embedding),
        "vector_search" => acall!(
            vectorstore::vector_search,
            project_path,
            query_embedding,
            top_k,
        ),
        "vector_delete" => acall!(vectorstore::vector_delete, project_path, page_id),
        "vector_count" => acall!(vectorstore::vector_count, project_path),
        "vector_upsert_chunks" => acall!(
            vectorstore::vector_upsert_chunks,
            project_path,
            page_id,
            chunks,
        ),
        "vector_search_chunks" => acall!(
            vectorstore::vector_search_chunks,
            project_path,
            query_embedding,
            top_k,
        ),
        "vector_delete_page" => acall!(vectorstore::vector_delete_page, project_path, page_id),
        "vector_count_chunks" => acall!(vectorstore::vector_count_chunks, project_path),
        "vector_clear_chunks" => acall!(vectorstore::vector_clear_chunks, project_path),
        "vector_optimize_chunks" => acall!(vectorstore::vector_optimize_chunks, project_path),
        "vector_legacy_row_count" => acall!(vectorstore::vector_legacy_row_count, project_path),
        "vector_drop_legacy" => acall!(vectorstore::vector_drop_legacy, project_path),

        // ---- CLI transports ----
        "claude_cli_detect" => acall!(claude_cli::claude_cli_detect),
        "claude_cli_spawn" => {
            let stream_id = peek_string(&args, "stream_id")?;
            register_cli_stream_forwarders(app, "claude-cli", &stream_id);
            acall_app_state!(
                claude_cli::claude_cli_spawn,
                stream_id,
                model,
                messages,
                isolate_local_config,
                working_directory,
            )
        }
        "claude_cli_kill" => acall_state!(claude_cli::claude_cli_kill, stream_id),
        "codex_cli_detect" => acall!(codex_cli::codex_cli_detect),
        "codex_cli_spawn" => {
            let stream_id = peek_string(&args, "stream_id")?;
            register_cli_stream_forwarders(app, "codex-cli", &stream_id);
            acall_app_state!(
                codex_cli::codex_cli_spawn,
                stream_id,
                model,
                prompt,
                isolate_local_config,
                timeout_minutes,
                working_directory,
            )
        }
        "codex_cli_kill" => acall_state!(codex_cli::codex_cli_kill, stream_id),

        // ---- image extraction ----
        "extract_pdf_images_cmd" => acall!(extract_images::extract_pdf_images_cmd, path),
        "extract_office_images_cmd" => acall!(extract_images::extract_office_images_cmd, path),
        "extract_and_save_pdf_images_cmd" => acall!(
            extract_images::extract_and_save_pdf_images_cmd,
            source_path,
            dest_dir,
            rel_to,
        ),
        "extract_and_save_office_images_cmd" => acall!(
            extract_images::extract_and_save_office_images_cmd,
            source_path,
            dest_dir,
            rel_to,
        ),

        // ---- file sync ----
        "start_project_file_watcher" => {
            let project_id: String = take_arg(&mut args, "project_id")?;
            let project_path: String = take_arg(&mut args, "project_path")?;
            let source_watch_config = take_arg(&mut args, "source_watch_config")?;
            json_result(file_sync::start_project_file_watcher(
                app.clone(),
                app.state(),
                project_id,
                project_path,
                source_watch_config,
            ))
        }
        "stop_project_file_watcher" => {
            json_result(file_sync::stop_project_file_watcher(app.state()))
        }
        "rescan_project_files" => scall_app!(
            file_sync::rescan_project_files,
            project_id,
            project_path,
            source_watch_config,
        ),
        "get_file_change_queue" => scall!(file_sync::get_file_change_queue, project_path),
        "retry_file_change_task" => scall_app!(
            file_sync::retry_file_change_task,
            project_id,
            project_path,
            task_id,
        ),
        "ignore_file_change_task" => scall_app!(
            file_sync::ignore_file_change_task,
            project_id,
            project_path,
            task_id,
        ),

        _ => Err(DispatchError::Unknown),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tauri::Emitter;

    #[test]
    fn snake_to_camel_matches_tauri_convention() {
        assert_eq!(snake_to_camel("extract_images"), "extractImages");
        assert_eq!(snake_to_camel("path"), "path");
        assert_eq!(snake_to_camel("source_watch_config"), "sourceWatchConfig");
    }

    #[test]
    fn mock_runtime_delivers_emitted_events_to_rust_listeners() {
        let app = tauri::test::mock_builder()
            .build(tauri::generate_context!())
            .expect("build mock app");
        let received = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = received.clone();
        app.listen_any("web-server-test", move |event| {
            sink.lock().unwrap().push(event.payload().to_string());
        });
        app.emit("web-server-test", serde_json::json!({ "hello": "world" }))
            .expect("emit");
        let received = received.lock().unwrap();
        assert_eq!(received.len(), 1, "listener should observe the emitted event");
        assert!(received[0].contains("hello"));
    }
}
