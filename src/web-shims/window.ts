/**
 * Web replacement for `@tauri-apps/api/window`.
 * Only the theme sync helpers are referenced (behind an isTauriRuntime()
 * guard that is false in the browser), so no-ops are sufficient.
 */

export type Theme = "light" | "dark"

class WebWindow {
  async setTheme(_theme: Theme | null): Promise<void> {}
  async setBackgroundColor(_color: unknown): Promise<void> {}
  async setTitle(title: string): Promise<void> {
    document.title = title
  }
}

const current = new WebWindow()

export function getCurrentWindow(): WebWindow {
  return current
}
