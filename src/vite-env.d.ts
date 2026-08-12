/// <reference types="vite/client" />

/** App version injected by Vite's `define` from package.json. */
declare const __APP_VERSION__: string

/**
 * True in web-mode builds (`vite build --mode web`), where the app runs in
 * a browser against llm-wiki-server instead of inside Tauri. Undefined in
 * desktop builds and tests.
 */
declare const __LLM_WIKI_WEB__: boolean | undefined
