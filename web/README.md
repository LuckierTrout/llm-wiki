# LLM Wiki — Web (self-hosted)

This repository is a derivative of [nashsu/llm_wiki](https://github.com/nashsu/llm_wiki)
(GPL-3.0) that adds a **web deployment mode**: the complete desktop app —
same React frontend, same Rust backend, all 77 IPC commands — served over
HTTP so it can run on a server and be used from any browser.

## Quick start (Docker)

```bash
export LLM_WIKI_WEB_PASSWORD=change-me
docker compose up --build -d
# open http://localhost:8080 and enter the password
```

All state (settings, wiki projects, uploads) lives in the `llm-wiki-data`
volume under `/data`.

Optional: **email documents into your wiki** — a small IMAP-polling sidecar
saves attachments from allowlisted senders straight into `raw/sources/`
for auto-ingestion. See [email-ingest/README.md](../email-ingest/README.md);
enable it with `docker compose --profile email up -d`.

## Quick start (bare metal)

```bash
# 1. Build the MCP server (the Rust build embeds it) and the web frontend
npm ci && npm --prefix mcp-server ci
npm run mcp:build && npm run build:web

# 2. Build the headless server
cd src-tauri
cargo build --release --features web-server --bin llm-wiki-server

# 3. Run it (frontend is found in ./dist relative to the working directory)
cd ..
LLM_WIKI_WEB_PASSWORD=change-me \
PDFIUM_DYNAMIC_LIB_PATH=$PWD/src-tauri/pdfium/libpdfium.so \
./src-tauri/target/release/llm-wiki-server
```

## How it works

The desktop app is a Tauri v2 application: a React frontend invoking ~77
Rust commands over Tauri IPC, plus a Tauri event channel for streaming.
The web mode reuses all of that code:

- **`llm-wiki-server`** (`src-tauri/src/web_server.rs`, behind the
  `web-server` cargo feature) builds the app on Tauri's headless
  `MockRuntime` — no display, no webview — and exposes:
  - `POST /api/invoke/{command}` — dispatches to the identical Rust
    command functions with the same managed state as the desktop app
  - `GET /api/events` — one SSE stream carrying every backend event
    (`agent-event`, `file-sync://*`, `claude-cli:*`, `codex-cli:*`)
  - `GET /api/asset?path=…` — replaces the `asset:` protocol for
    images/PDF/media previews
  - `POST /api/http-proxy` — Rust-side fetch replacing
    `tauri-plugin-http`, so LLM endpoints without browser CORS headers
    keep working; responses stream token-by-token
  - `GET|POST /api/store/{file}` — persists `app-state.json` server-side
    (the same file the Rust backend reads for proxy/API config)
  - `POST /api/upload`, `GET /api/download`, `GET /api/browse` — file
    import, archive export, and the in-browser folder picker
  - `/api/v1/*` and `/clip/*` — reverse proxies to the embedded local
    HTTP API (`:19828`) and Web-Clipper server (`:19827`), which both run
    inside the same process exactly like on desktop
  - everything else — the built frontend (SPA fallback)
- **Web shims** (`src/web-shims/`, aliased in `vite.config.ts` when
  building with `--mode web`) replace the `@tauri-apps/*` packages in the
  browser: `invoke` → HTTP, `listen` → SSE, dialogs → server-side folder
  picker / file upload, store → server store API, opener → new tab,
  autostart/window → no-ops.

## Configuration (environment variables)

| Variable | Default | Purpose |
|---|---|---|
| `LLM_WIKI_WEB_PASSWORD` | *(unset)* | **Set this.** Password for the web UI/API (HttpOnly session cookie or `Authorization: Bearer`). Unset = unauthenticated. |
| `LLM_WIKI_WEB_PORT` / `PORT` | `8080` | HTTP port |
| `LLM_WIKI_WEB_BIND` | `0.0.0.0` | Bind address |
| `LLM_WIKI_WEB_ROOT` | `./dist` next to binary or cwd | Frontend build directory |
| `LLM_WIKI_PROJECTS_DIR` | `<app data>/projects` | Default root shown by the folder picker |
| `PDFIUM_DYNAMIC_LIB_PATH` | auto-detected | Explicit path to `libpdfium.so` for PDF parsing |
| `LLM_WIKI_API_TOKEN` | *(unset)* | Token for the embedded `:19828` HTTP API / MCP access |
| `LLM_WIKI_BIND_HOST` | `127.0.0.1` | Bind host for the embedded API/clipper servers |
| `HOME` / `XDG_DATA_HOME` | — | Controls the app-data location (`…/com.llmwiki.app/app-state.json`) |

## Security model

The password protects **everything**: whoever holds it has the same power
a desktop user has — full read/write access to files the server process
can reach, outbound HTTP through the proxy endpoint, and (if the CLIs are
installed) spawning `claude`/`codex` subprocesses. Treat it accordingly:

- Always set `LLM_WIKI_WEB_PASSWORD` before exposing the port.
- This is a **single-user** app (one project watcher, one clip queue,
  global settings) — don't share one instance between people.
- Put TLS in front (Caddy, nginx, Traefik) for anything non-local; the
  session cookie is not marked `Secure` by the server itself.
- The Docker container is the recommended isolation boundary.

## Development loop

```bash
# Terminal 1 — backend (rebuild on Rust changes)
cd src-tauri && cargo run --features web-server --bin llm-wiki-server

# Terminal 2 — frontend with hot reload, proxying /api + /clip to :8080
npm run dev:web   # → http://localhost:1420
```

## Web-mode limitations

- "Reveal in file manager" commands return a friendly error (no desktop).
- Autostart/tray/window-close behaviors are no-ops.
- Files "open with system app" via a browser tab instead.
- Claude Code / Codex CLI chat transports work only if those CLIs are
  installed in the server environment (not included in the Docker image).
- The Chrome web-clipper extension posts to `127.0.0.1:19827` on the
  machine running the *browser*; point it at your server's `/clip/…`
  routes (same token) if you want clipping into the hosted instance.
