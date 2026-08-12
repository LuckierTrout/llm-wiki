import { isWebMode } from "@/lib/runtime-mode"

export const API_SERVER_PORT = 19828
/**
 * On desktop the HTTP API daemon listens on localhost. On web the same
 * daemon runs inside llm-wiki-server and is reverse-proxied under the page
 * origin, so relative URLs keep every consumer working unchanged.
 */
export const API_SERVER_BASE_URL = isWebMode() ? "" : `http://127.0.0.1:${API_SERVER_PORT}`
export const API_SERVER_HEALTH_URL = `${API_SERVER_BASE_URL}/api/v1/health`
