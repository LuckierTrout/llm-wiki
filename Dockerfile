# LLM Wiki — web deployment image.
#
# Builds the browser frontend (Vite `web` mode) and the headless Rust server
# (`llm-wiki-server`, the full Tauri command surface over HTTP + SSE), then
# packages both on a slim Debian base.
#
#   docker build -t llm-wiki-web .
#   docker run -p 8080:8080 -e LLM_WIKI_WEB_PASSWORD=change-me \
#     -v llm-wiki-data:/data llm-wiki-web

# ---------------------------------------------------------------------------
# Stage 1: frontend + MCP server build
# ---------------------------------------------------------------------------
FROM node:22-bookworm-slim AS frontend
WORKDIR /app
COPY package.json package-lock.json ./
COPY mcp-server/package.json mcp-server/package-lock.json mcp-server/
RUN npm ci --no-audit --no-fund \
    && npm --prefix mcp-server ci --no-audit --no-fund
COPY . .
# mcp-server/dist must exist before the Rust build (tauri-build bundles it),
# and build:web produces the browser bundle in dist/.
RUN npm run mcp:build && npm run build:web

# ---------------------------------------------------------------------------
# Stage 2: Rust server build
# ---------------------------------------------------------------------------
FROM rust:1-bookworm AS backend
RUN apt-get update && apt-get install -y --no-install-recommends \
        libgtk-3-dev \
        libwebkit2gtk-4.1-dev \
        libayatana-appindicator3-dev \
        librsvg2-dev \
        libxdo-dev \
        libssl-dev \
        protobuf-compiler \
        pkg-config \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=frontend /app /app
RUN cd src-tauri \
    && cargo build --release --features web-server --bin llm-wiki-server \
    && cp target/release/llm-wiki-server /usr/local/bin/llm-wiki-server

# ---------------------------------------------------------------------------
# Stage 3: runtime
# ---------------------------------------------------------------------------
FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y --no-install-recommends \
        ca-certificates \
        libwebkit2gtk-4.1-0 \
        libgtk-3-0 \
        libayatana-appindicator3-1 \
        libxdo3 \
    && rm -rf /var/lib/apt/lists/*

WORKDIR /opt/llm-wiki
COPY --from=backend /usr/local/bin/llm-wiki-server ./llm-wiki-server
# Browser frontend, served by the binary (resolved relative to the exe).
COPY --from=frontend /app/dist ./dist
# Bundled pdfium for PDF text/image extraction (found via the resource-dir
# hint: <exe dir>/pdfium/libpdfium.so).
COPY src-tauri/pdfium/libpdfium.so ./pdfium/libpdfium.so
# MCP server sources so `mcp_server_entry_path` can point clients at them
# (requires Node on the connecting machine or in a derived image).
COPY --from=frontend /app/mcp-server/dist ./mcp-server/dist
COPY --from=frontend /app/mcp-server/package.json ./mcp-server/package.json
COPY --from=frontend /app/mcp-server/node_modules ./mcp-server/node_modules

# All persistent state lives under /data:
#   /data/appdata/com.llmwiki.app/app-state.json  — settings (also read by Rust)
#   /data/projects                                 — default wiki project root
ENV HOME=/data \
    XDG_DATA_HOME=/data/appdata \
    XDG_CONFIG_HOME=/data/config \
    XDG_CACHE_HOME=/data/cache \
    LLM_WIKI_PROJECTS_DIR=/data/projects \
    LLM_WIKI_WEB_PORT=8080 \
    LLM_WIKI_WEB_BIND=0.0.0.0
VOLUME ["/data"]
EXPOSE 8080

ENTRYPOINT ["/opt/llm-wiki/llm-wiki-server"]
