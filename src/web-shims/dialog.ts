/**
 * Web replacement for `@tauri-apps/plugin-dialog`.
 *
 * - Directory pickers become a small in-page browser over the server's
 *   filesystem (`GET /api/browse`), since projects live on the server.
 * - File pickers use a real `<input type="file">` and upload the chosen
 *   files to the server (`POST /api/upload`), returning server-side
 *   absolute paths so the existing import flow (copy_file into the
 *   project) works unchanged.
 * - `save` is a directory picker plus a file-name field.
 * - `message` falls back to `alert`.
 */

import { apiFetch } from "./client"
import { invoke } from "./core"

interface DialogFilter {
  name: string
  extensions: string[]
}

export interface OpenDialogOptions {
  title?: string
  directory?: boolean
  multiple?: boolean
  filters?: DialogFilter[]
  defaultPath?: string
  createDirectories?: boolean
  [key: string]: unknown
}

export interface SaveDialogOptions {
  title?: string
  defaultPath?: string
  filters?: DialogFilter[]
  [key: string]: unknown
}

interface BrowseResponse {
  ok: boolean
  path: string
  parent: string | null
  dirs: string[]
  sep: string
}

async function browse(path: string): Promise<BrowseResponse | null> {
  const response = await apiFetch(`/api/browse?path=${encodeURIComponent(path)}`)
  if (!response.ok) {
    return null
  }
  return (await response.json()) as BrowseResponse
}

function joinPath(dir: string, sep: string, name: string): string {
  return dir.endsWith(sep) || dir.endsWith("/") ? `${dir}${name}` : `${dir}${sep}${name}`
}

const STYLES = {
  overlay:
    "position:fixed;inset:0;z-index:2147482000;display:flex;align-items:center;justify-content:center;background:rgba(15,17,21,0.66);font-family:system-ui,sans-serif",
  card: "background:#1d1f24;color:#e8eaed;border:1px solid #34363c;border-radius:12px;padding:20px;width:520px;max-width:calc(100vw - 48px);box-shadow:0 24px 64px rgba(0,0,0,0.5);display:flex;flex-direction:column;gap:12px",
  title: "font-size:15px;font-weight:600",
  pathRow: "display:flex;gap:8px",
  pathInput:
    "flex:1;padding:8px 10px;border-radius:8px;border:1px solid #3c4043;background:#111318;color:#e8eaed;font-size:12.5px;font-family:ui-monospace,monospace;outline:none",
  list: "height:240px;overflow:auto;border:1px solid #2c2e33;border-radius:8px;background:#15171b;padding:4px",
  entry:
    "display:block;width:100%;text-align:left;padding:7px 10px;border:none;border-radius:6px;background:transparent;color:#dfe1e5;font-size:13px;cursor:pointer",
  nameInput:
    "width:100%;box-sizing:border-box;padding:8px 10px;border-radius:8px;border:1px solid #3c4043;background:#111318;color:#e8eaed;font-size:13px;outline:none",
  buttonRow: "display:flex;justify-content:space-between;gap:8px;align-items:center",
  button:
    "padding:8px 14px;border-radius:8px;border:1px solid #3c4043;background:#26282e;color:#e8eaed;font-size:13px;cursor:pointer",
  primary:
    "padding:8px 14px;border-radius:8px;border:none;background:#8ab4f8;color:#17191d;font-size:13px;font-weight:600;cursor:pointer",
} as const

interface PickPathOptions {
  title: string
  startPath?: string
  fileName?: string
}

/** In-page directory browser over the server filesystem. */
function pickServerPath(options: PickPathOptions): Promise<string | null> {
  return new Promise((resolve) => {
    let currentPath = ""
    let currentSep = "/"

    const overlay = document.createElement("div")
    overlay.setAttribute("style", STYLES.overlay)
    const card = document.createElement("div")
    card.setAttribute("style", STYLES.card)

    const title = document.createElement("div")
    title.textContent = options.title
    title.setAttribute("style", STYLES.title)

    const pathRow = document.createElement("div")
    pathRow.setAttribute("style", STYLES.pathRow)
    const pathInput = document.createElement("input")
    pathInput.setAttribute("style", STYLES.pathInput)
    pathInput.spellcheck = false
    const goButton = document.createElement("button")
    goButton.textContent = "Go"
    goButton.setAttribute("style", STYLES.button)
    pathRow.append(pathInput, goButton)

    const list = document.createElement("div")
    list.setAttribute("style", STYLES.list)

    let nameInput: HTMLInputElement | null = null
    if (options.fileName !== undefined) {
      nameInput = document.createElement("input")
      nameInput.value = options.fileName
      nameInput.setAttribute("style", STYLES.nameInput)
      nameInput.spellcheck = false
    }

    const buttonRow = document.createElement("div")
    buttonRow.setAttribute("style", STYLES.buttonRow)
    const newFolderButton = document.createElement("button")
    newFolderButton.textContent = "New folder"
    newFolderButton.setAttribute("style", STYLES.button)
    const rightGroup = document.createElement("div")
    rightGroup.setAttribute("style", "display:flex;gap:8px")
    const cancelButton = document.createElement("button")
    cancelButton.textContent = "Cancel"
    cancelButton.setAttribute("style", STYLES.button)
    const selectButton = document.createElement("button")
    selectButton.textContent = "Select"
    selectButton.setAttribute("style", STYLES.primary)
    rightGroup.append(cancelButton, selectButton)
    buttonRow.append(newFolderButton, rightGroup)

    card.append(title, pathRow, list)
    if (nameInput) {
      card.append(nameInput)
    }
    card.append(buttonRow)
    overlay.append(card)

    const finish = (value: string | null) => {
      overlay.remove()
      resolve(value)
    }

    const renderEntries = (data: BrowseResponse) => {
      currentPath = data.path
      currentSep = data.sep || "/"
      pathInput.value = data.path
      list.replaceChildren()
      if (data.parent) {
        const up = document.createElement("button")
        up.textContent = "⬆ .."
        up.setAttribute("style", STYLES.entry)
        up.addEventListener("click", () => void navigate(data.parent as string))
        list.append(up)
      }
      for (const dir of data.dirs) {
        const entry = document.createElement("button")
        entry.textContent = `\u{1F4C1} ${dir}`
        entry.setAttribute("style", STYLES.entry)
        entry.addEventListener("mouseenter", () => (entry.style.background = "#26282e"))
        entry.addEventListener("mouseleave", () => (entry.style.background = "transparent"))
        entry.addEventListener("click", () =>
          void navigate(joinPath(currentPath, currentSep, dir)),
        )
        list.append(entry)
      }
      if (!data.dirs.length && !data.parent) {
        const empty = document.createElement("div")
        empty.textContent = "No subfolders"
        empty.setAttribute("style", "padding:8px 10px;color:#9aa0a6;font-size:12.5px")
        list.append(empty)
      }
    }

    const navigate = async (path: string) => {
      const data = await browse(path)
      if (data?.ok) {
        renderEntries(data)
      }
    }

    goButton.addEventListener("click", () => void navigate(pathInput.value))
    pathInput.addEventListener("keydown", (event) => {
      if (event.key === "Enter") {
        void navigate(pathInput.value)
      }
    })
    newFolderButton.addEventListener("click", async () => {
      const name = window.prompt("New folder name")
      if (!name) {
        return
      }
      const target = joinPath(currentPath, currentSep, name)
      try {
        await invoke("create_directory", { path: target })
        await navigate(target)
      } catch (error) {
        window.alert(`Could not create folder: ${String(error)}`)
      }
    })
    cancelButton.addEventListener("click", () => finish(null))
    selectButton.addEventListener("click", () => {
      if (nameInput) {
        const name = nameInput.value.trim()
        if (!name) {
          return
        }
        finish(joinPath(currentPath, currentSep, name))
      } else {
        finish(currentPath)
      }
    })
    overlay.addEventListener("keydown", (event) => {
      if (event.key === "Escape") {
        finish(null)
      }
    })

    document.body.append(overlay)
    void navigate(options.startPath ?? "")
  })
}

function acceptFromFilters(filters?: DialogFilter[]): string | undefined {
  if (!filters?.length) {
    return undefined
  }
  const extensions = filters
    .flatMap((filter) => filter.extensions)
    .filter((ext) => ext && ext !== "*")
    .map((ext) => (ext.startsWith(".") ? ext : `.${ext}`))
  return extensions.length ? extensions.join(",") : undefined
}

function pickLocalFiles(options: OpenDialogOptions): Promise<File[] | null> {
  return new Promise((resolve) => {
    const input = document.createElement("input")
    input.type = "file"
    input.multiple = options.multiple !== false && options.multiple !== undefined
      ? Boolean(options.multiple)
      : false
    const accept = acceptFromFilters(options.filters)
    if (accept) {
      input.accept = accept
    }
    input.style.display = "none"
    input.addEventListener("change", () => {
      const files = input.files ? Array.from(input.files) : []
      input.remove()
      resolve(files.length ? files : null)
    })
    input.addEventListener("cancel", () => {
      input.remove()
      resolve(null)
    })
    document.body.append(input)
    input.click()
  })
}

async function uploadFile(file: File): Promise<string> {
  const response = await apiFetch(`/api/upload?name=${encodeURIComponent(file.name)}`, {
    method: "POST",
    body: file,
  })
  const data = (await response.json()) as { ok?: boolean; path?: string; error?: string }
  if (!response.ok || !data.ok || !data.path) {
    throw data.error ?? `upload of ${file.name} failed`
  }
  return data.path
}

export async function open(
  options: OpenDialogOptions = {},
): Promise<string | string[] | null> {
  if (options.directory) {
    const picked = await pickServerPath({
      title: options.title ?? "Select folder",
      startPath: options.defaultPath,
    })
    if (picked === null) {
      return null
    }
    return options.multiple ? [picked] : picked
  }
  const files = await pickLocalFiles(options)
  if (!files) {
    return null
  }
  const paths: string[] = []
  for (const file of files) {
    paths.push(await uploadFile(file))
  }
  return options.multiple ? paths : paths[0] ?? null
}

export async function save(options: SaveDialogOptions = {}): Promise<string | null> {
  const defaultName = options.defaultPath?.split(/[\\/]/).pop() ?? "export"
  return pickServerPath({
    title: options.title ?? "Save as",
    fileName: defaultName,
  })
}

export async function message(text: string, _options?: unknown): Promise<void> {
  window.alert(text)
}

export async function ask(text: string, _options?: unknown): Promise<boolean> {
  return window.confirm(text)
}

export async function confirm(text: string, _options?: unknown): Promise<boolean> {
  return window.confirm(text)
}
