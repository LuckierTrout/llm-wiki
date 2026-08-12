/**
 * Web replacement for `@tauri-apps/api/core`.
 *
 * `invoke` maps 1:1 onto the server's `POST /api/invoke/{command}` route,
 * which dispatches to the identical Rust command functions the desktop app
 * registers. Errors reject with the command's error string, matching
 * Tauri's IPC contract.
 */

import { apiFetch } from "./client"

export async function invoke<T>(
  command: string,
  args?: Record<string, unknown>,
  _options?: unknown,
): Promise<T> {
  const response = await apiFetch(`/api/invoke/${encodeURIComponent(command)}`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify(args ?? {}),
  })
  let data: { ok?: boolean; value?: T; error?: string } | null = null
  try {
    data = (await response.json()) as { ok?: boolean; value?: T; error?: string }
  } catch {
    data = null
  }
  if (response.ok && data?.ok) {
    return data.value as T
  }
  throw data?.error ?? `invoke(${command}) failed with HTTP ${response.status}`
}

/** Serve local files through the web server instead of the asset: protocol. */
export function convertFileSrc(filePath: string, _protocol = "asset"): string {
  return `/api/asset?path=${encodeURIComponent(filePath)}`
}

export function isTauri(): boolean {
  return false
}
