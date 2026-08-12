/**
 * Web replacement for `@tauri-apps/plugin-http`.
 *
 * On desktop the plugin executes HTTP from Rust so third-party LLM
 * endpoints without browser-friendly CORS headers still work. The web
 * build keeps that property by relaying requests through the server's
 * `POST /api/http-proxy`, which performs the request with reqwest and
 * streams the body back. Upstream status/headers travel in
 * `X-Proxy-Status` / `X-Proxy-Headers` so a faithful `Response` can be
 * reconstructed even for upstream error statuses.
 */

import { apiFetch } from "./client"

const NULL_BODY_STATUSES = new Set([101, 204, 205, 304])

function encodeBase64(bytes: Uint8Array): string {
  let binary = ""
  const chunkSize = 0x8000
  for (let i = 0; i < bytes.length; i += chunkSize) {
    binary += String.fromCharCode(...bytes.subarray(i, i + chunkSize))
  }
  return btoa(binary)
}

function normalizeHeaders(init?: HeadersInit): Array<[string, string]> {
  if (!init) {
    return []
  }
  if (init instanceof Headers) {
    return [...init.entries()]
  }
  if (Array.isArray(init)) {
    return init.map(([name, value]) => [String(name), String(value)])
  }
  return Object.entries(init).map(([name, value]) => [name, String(value)])
}

async function serializeBody(body: BodyInit | null | undefined): Promise<string | null> {
  if (body === null || body === undefined) {
    return null
  }
  if (typeof body === "string") {
    return encodeBase64(new TextEncoder().encode(body))
  }
  if (body instanceof Uint8Array) {
    return encodeBase64(body)
  }
  if (body instanceof ArrayBuffer) {
    return encodeBase64(new Uint8Array(body))
  }
  if (body instanceof Blob) {
    return encodeBase64(new Uint8Array(await body.arrayBuffer()))
  }
  if (body instanceof URLSearchParams) {
    return encodeBase64(new TextEncoder().encode(body.toString()))
  }
  throw new TypeError("[web-shim] unsupported request body type for proxied fetch")
}

export async function fetch(
  input: string | URL | Request,
  init?: RequestInit,
): Promise<Response> {
  let url: string
  let method = init?.method ?? "GET"
  let headers = normalizeHeaders(init?.headers)
  let bodyB64: string | null = null

  if (input instanceof Request) {
    url = input.url
    method = init?.method ?? input.method
    headers = headers.length ? headers : [...input.headers.entries()]
    if (init?.body !== undefined) {
      bodyB64 = await serializeBody(init.body)
    } else {
      const buffer = await input.clone().arrayBuffer()
      bodyB64 = buffer.byteLength ? encodeBase64(new Uint8Array(buffer)) : null
    }
  } else {
    url = String(input)
    bodyB64 = await serializeBody(init?.body)
  }

  const response = await apiFetch("/api/http-proxy", {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({
      url,
      method,
      headers,
      body_b64: bodyB64,
    }),
    signal: init?.signal ?? undefined,
  })

  const proxyStatus = response.headers.get("x-proxy-status")
  if (!proxyStatus) {
    // The proxy itself failed (network error, bad request…) — behave like
    // a browser network failure so caller retry logic engages.
    let detail = `proxy request failed with HTTP ${response.status}`
    try {
      const parsed = (await response.json()) as { error?: string }
      if (parsed.error) {
        detail = parsed.error
      }
    } catch {
      // keep default detail
    }
    throw new TypeError(detail)
  }

  const status = Number.parseInt(proxyStatus, 10) || 502
  const upstreamHeaders = new Headers()
  const encodedHeaders = response.headers.get("x-proxy-headers")
  if (encodedHeaders) {
    try {
      const parsed = JSON.parse(atob(encodedHeaders)) as Array<[string, string]>
      for (const [name, value] of parsed) {
        try {
          upstreamHeaders.append(name, value)
        } catch {
          // skip headers the browser refuses to set programmatically
        }
      }
    } catch {
      // ignore malformed header envelope
    }
  }

  const body = NULL_BODY_STATUSES.has(status) ? null : response.body
  return new Response(body, { status, headers: upstreamHeaders })
}
