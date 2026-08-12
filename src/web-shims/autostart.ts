/**
 * Web replacement for `@tauri-apps/plugin-autostart`.
 * Autostart has no meaning for a hosted server — the container/systemd unit
 * owns process lifecycle — so these are safe no-ops.
 */

export async function enable(): Promise<void> {}

export async function disable(): Promise<void> {}

export async function isEnabled(): Promise<boolean> {
  return false
}
