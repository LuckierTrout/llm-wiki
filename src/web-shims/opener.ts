/**
 * Web replacement for `@tauri-apps/plugin-opener`.
 *
 * URLs open in a new tab. Local paths are served through the web server's
 * asset route, which lets the browser preview or download them — the
 * closest browser equivalent of "open with the system default app".
 */

export async function openUrl(url: string | URL, _openWith?: string): Promise<void> {
  window.open(String(url), "_blank", "noopener,noreferrer")
}

export async function openPath(path: string, _openWith?: string): Promise<void> {
  window.open(`/api/asset?path=${encodeURIComponent(path)}`, "_blank", "noopener,noreferrer")
}

export async function revealItemInDir(path: string): Promise<void> {
  return openPath(path)
}
