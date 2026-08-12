/**
 * Distinguishes the web deployment (llm-wiki-server + browser) from the
 * Tauri desktop app. `__LLM_WIKI_WEB__` is injected by Vite's `define`
 * when building with `--mode web`; it is absent in tests and desktop
 * builds, so guard the access.
 */
export function isWebMode(): boolean {
  return typeof __LLM_WIKI_WEB__ !== "undefined" && __LLM_WIKI_WEB__ === true
}

/**
 * Base URL for the clip server. On desktop the frontend talks straight to
 * the local daemon; on web the same daemon runs inside the server process
 * and is reached through the `/clip` reverse proxy on the page origin.
 */
export function clipServerBase(): string {
  return isWebMode() ? "/clip" : "http://127.0.0.1:19827"
}
