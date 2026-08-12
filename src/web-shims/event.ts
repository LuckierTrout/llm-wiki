/**
 * Web replacement for `@tauri-apps/api/event`.
 *
 * The server broadcasts every backend event on a single SSE stream
 * (`GET /api/events`) using the Tauri event name as the SSE event name.
 * `listen()` attaches per-name handlers to one shared EventSource, so —
 * like on desktop — listeners registered before a command is invoked are
 * guaranteed to observe every event that command emits.
 */

export type UnlistenFn = () => void

export interface Event<T> {
  event: string
  id: number
  payload: T
}

type Handler = (event: MessageEvent) => void

let source: EventSource | null = null
let sequence = 0
const registered = new Map<string, Set<{ name: string; handler: Handler }>>()

function ensureSource(): EventSource {
  if (source) {
    return source
  }
  source = new EventSource("/api/events")
  for (const entries of registered.values()) {
    for (const entry of entries) {
      source.addEventListener(entry.name, entry.handler)
    }
  }
  return source
}

function reconnect() {
  if (source) {
    source.close()
    source = null
  }
  ensureSource()
}

if (typeof window !== "undefined") {
  // Re-establish the stream once the user authenticates (the initial
  // connection is rejected while the session cookie is missing).
  window.addEventListener("llmwiki:auth-changed", reconnect)
}

export async function listen<T>(
  event: string,
  handler: (event: Event<T>) => void,
): Promise<UnlistenFn> {
  const domHandler: Handler = (message) => {
    let payload: T
    try {
      payload = JSON.parse(message.data) as T
    } catch {
      payload = message.data as T
    }
    handler({ event, id: ++sequence, payload })
  }
  const entry = { name: event, handler: domHandler }
  if (!registered.has(event)) {
    registered.set(event, new Set())
  }
  registered.get(event)?.add(entry)
  ensureSource().addEventListener(event, domHandler)
  return () => {
    registered.get(event)?.delete(entry)
    source?.removeEventListener(event, domHandler)
  }
}

export async function once<T>(
  event: string,
  handler: (event: Event<T>) => void,
): Promise<UnlistenFn> {
  const unlisten = await listen<T>(event, (payload) => {
    unlisten()
    handler(payload)
  })
  return unlisten
}

export async function emit(_event: string, _payload?: unknown): Promise<void> {
  // Nothing in the app emits frontend->backend events; commands cover it.
  console.warn("[web-shim] emit() is not supported in the web build")
}
