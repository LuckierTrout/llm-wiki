/**
 * Shared HTTP plumbing for the web build's Tauri API shims.
 *
 * All shims talk to the llm-wiki-server process that serves this page
 * (same origin). Authentication uses an HttpOnly session cookie set by
 * POST /api/login; when a request comes back 401 a minimal password
 * overlay is shown and the request retried.
 */

export const API_BASE = ""

let loginPromise: Promise<void> | null = null

export async function apiFetch(path: string, init?: RequestInit): Promise<Response> {
  const response = await fetch(`${API_BASE}${path}`, {
    credentials: "same-origin",
    ...init,
  })
  if (response.status !== 401) {
    return response
  }
  await requestLogin()
  return fetch(`${API_BASE}${path}`, { credentials: "same-origin", ...init })
}

export function requestLogin(): Promise<void> {
  if (!loginPromise) {
    loginPromise = showLoginOverlay().finally(() => {
      loginPromise = null
    })
  }
  return loginPromise
}

async function submitPassword(password: string): Promise<boolean> {
  try {
    const response = await fetch(`${API_BASE}/api/login`, {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ password }),
    })
    return response.ok
  } catch {
    return false
  }
}

function showLoginOverlay(): Promise<void> {
  return new Promise((resolve) => {
    const overlay = document.createElement("div")
    overlay.setAttribute(
      "style",
      [
        "position:fixed",
        "inset:0",
        "z-index:2147483000",
        "display:flex",
        "align-items:center",
        "justify-content:center",
        "background:rgba(15,17,21,0.72)",
        "backdrop-filter:blur(4px)",
        "font-family:system-ui,sans-serif",
      ].join(";"),
    )

    const card = document.createElement("div")
    card.setAttribute(
      "style",
      [
        "background:#1d1f24",
        "color:#e8eaed",
        "border:1px solid #34363c",
        "border-radius:12px",
        "padding:28px",
        "width:320px",
        "box-shadow:0 24px 64px rgba(0,0,0,0.5)",
      ].join(";"),
    )

    const title = document.createElement("div")
    title.textContent = "LLM Wiki"
    title.setAttribute("style", "font-size:18px;font-weight:600;margin-bottom:6px")

    const subtitle = document.createElement("div")
    subtitle.textContent = "Enter the server password to continue."
    subtitle.setAttribute("style", "font-size:13px;color:#9aa0a6;margin-bottom:16px")

    const input = document.createElement("input")
    input.type = "password"
    input.autocomplete = "current-password"
    input.placeholder = "Password"
    input.setAttribute(
      "style",
      [
        "width:100%",
        "box-sizing:border-box",
        "padding:10px 12px",
        "border-radius:8px",
        "border:1px solid #3c4043",
        "background:#111318",
        "color:#e8eaed",
        "font-size:14px",
        "outline:none",
      ].join(";"),
    )

    const error = document.createElement("div")
    error.setAttribute(
      "style",
      "font-size:12px;color:#f28b82;min-height:16px;margin-top:8px",
    )

    const button = document.createElement("button")
    button.textContent = "Unlock"
    button.setAttribute(
      "style",
      [
        "width:100%",
        "margin-top:12px",
        "padding:10px 12px",
        "border-radius:8px",
        "border:none",
        "background:#8ab4f8",
        "color:#17191d",
        "font-size:14px",
        "font-weight:600",
        "cursor:pointer",
      ].join(";"),
    )

    const submit = async () => {
      const password = input.value
      if (!password) {
        return
      }
      button.disabled = true
      error.textContent = ""
      const ok = await submitPassword(password)
      button.disabled = false
      if (!ok) {
        error.textContent = "Incorrect password."
        input.select()
        return
      }
      overlay.remove()
      window.dispatchEvent(new CustomEvent("llmwiki:auth-changed"))
      resolve()
    }

    button.addEventListener("click", submit)
    input.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        void submit()
      }
    })

    card.append(title, subtitle, input, error, button)
    overlay.append(card)
    document.body.append(overlay)
    input.focus()
  })
}
