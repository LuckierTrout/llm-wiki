/**
 * Web replacement for `@tauri-apps/plugin-store`.
 *
 * Stores persist server-side under the app data directory — the same
 * `app-state.json` the Rust backend reads for `proxyConfig`, `apiConfig`
 * and the bind host, so that contract survives the web port unchanged.
 * Instances are singletons per file name, mirroring the plugin, so the
 * two modules that `load("app-state.json")` share one cache.
 */

import { apiFetch } from "./client"

const AUTOSAVE_DELAY_MS = 150

class WebStore {
  private cache: Record<string, unknown> | null = null
  private loading: Promise<void> | null = null
  private saveTimer: ReturnType<typeof setTimeout> | null = null

  constructor(
    private readonly name: string,
    private readonly autoSave: boolean,
    private readonly defaults: Record<string, unknown>,
  ) {}

  private async ensureLoaded(): Promise<Record<string, unknown>> {
    if (this.cache) {
      return this.cache
    }
    if (!this.loading) {
      this.loading = (async () => {
        const response = await apiFetch(`/api/store/${encodeURIComponent(this.name)}`)
        let parsed: Record<string, unknown> = {}
        try {
          const data = (await response.json()) as unknown
          if (data && typeof data === "object" && !Array.isArray(data)) {
            parsed = data as Record<string, unknown>
          }
        } catch {
          parsed = {}
        }
        this.cache = { ...this.defaults, ...parsed }
      })()
    }
    await this.loading
    return (this.cache ??= {})
  }

  private scheduleAutoSave() {
    if (!this.autoSave) {
      return
    }
    if (this.saveTimer) {
      clearTimeout(this.saveTimer)
    }
    this.saveTimer = setTimeout(() => {
      void this.save()
    }, AUTOSAVE_DELAY_MS)
  }

  async get<T>(key: string): Promise<T | undefined> {
    const cache = await this.ensureLoaded()
    return cache[key] as T | undefined
  }

  async set(key: string, value: unknown): Promise<void> {
    const cache = await this.ensureLoaded()
    cache[key] = value
    this.scheduleAutoSave()
  }

  async has(key: string): Promise<boolean> {
    const cache = await this.ensureLoaded()
    return key in cache
  }

  async delete(key: string): Promise<boolean> {
    const cache = await this.ensureLoaded()
    const existed = key in cache
    delete cache[key]
    this.scheduleAutoSave()
    return existed
  }

  async keys(): Promise<string[]> {
    return Object.keys(await this.ensureLoaded())
  }

  async values(): Promise<unknown[]> {
    return Object.values(await this.ensureLoaded())
  }

  async entries(): Promise<Array<[string, unknown]>> {
    return Object.entries(await this.ensureLoaded())
  }

  async length(): Promise<number> {
    return (await this.keys()).length
  }

  async clear(): Promise<void> {
    this.cache = {}
    this.scheduleAutoSave()
  }

  async reset(): Promise<void> {
    this.cache = { ...this.defaults }
    this.scheduleAutoSave()
  }

  async reload(): Promise<void> {
    this.cache = null
    this.loading = null
    await this.ensureLoaded()
  }

  async save(): Promise<void> {
    if (this.saveTimer) {
      clearTimeout(this.saveTimer)
      this.saveTimer = null
    }
    const cache = await this.ensureLoaded()
    await apiFetch(`/api/store/${encodeURIComponent(this.name)}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(cache),
    })
  }
}

const instances = new Map<string, WebStore>()

export interface StoreOptions {
  autoSave?: boolean | number
  defaults?: Record<string, unknown>
  [key: string]: unknown
}

export async function load(name: string, options?: StoreOptions): Promise<WebStore> {
  let store = instances.get(name)
  if (!store) {
    store = new WebStore(name, options?.autoSave !== false, options?.defaults ?? {})
    instances.set(name, store)
  }
  return store
}

export type Store = WebStore
