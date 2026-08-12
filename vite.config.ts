import path from "path"
import { readFileSync } from "fs"
import { defineConfig } from "vite"
import react from "@vitejs/plugin-react"
import tailwindcss from "@tailwindcss/vite"

const host = process.env.TAURI_DEV_HOST

// Read version from package.json at config-load time so the Settings
// UI can show the running app version without duplicating the string.
const pkgJson = JSON.parse(readFileSync(path.resolve(__dirname, "package.json"), "utf-8"))

// The `web` mode builds the same frontend for llm-wiki-server (the headless
// web deployment): every @tauri-apps package is swapped for an HTTP/SSE
// shim under src/web-shims, and API calls go to the page origin.
const webShim = (name: string) => path.resolve(__dirname, `./src/web-shims/${name}.ts`)
const webAliases = {
  "@tauri-apps/api/core": webShim("core"),
  "@tauri-apps/api/event": webShim("event"),
  "@tauri-apps/api/window": webShim("window"),
  "@tauri-apps/plugin-dialog": webShim("dialog"),
  "@tauri-apps/plugin-opener": webShim("opener"),
  "@tauri-apps/plugin-store": webShim("store"),
  "@tauri-apps/plugin-autostart": webShim("autostart"),
  "@tauri-apps/plugin-http": webShim("http"),
}

// https://vitejs.dev/config/
export default defineConfig(async ({ mode }) => {
  const isWeb = mode === "web"
  return {
    plugins: [react(), tailwindcss()],

    resolve: {
      alias: {
        "@": path.resolve(__dirname, "./src"),
        ...(isWeb ? webAliases : {}),
      },
    },

    define: {
      __APP_VERSION__: JSON.stringify(pkgJson.version),
      __LLM_WIKI_WEB__: JSON.stringify(isWeb),
    },

    // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
    //
    // 1. prevent vite from obscuring rust errors
    clearScreen: false,
    // 2. tauri expects a fixed port, fail if that port is not available
    server: {
      port: 1420,
      strictPort: true,
      host: host || false,
      hmr: host
        ? {
            protocol: "ws",
            host,
            port: 1421,
          }
        : undefined,
      watch: {
        // 3. tell vite to ignore watching `src-tauri`
        ignored: ["**/src-tauri/**"],
      },
      // In web dev mode, forward API traffic to a locally running
      // llm-wiki-server (`cargo run --features web-server --bin llm-wiki-server`).
      proxy: isWeb
        ? {
            "/api": { target: "http://127.0.0.1:8080", changeOrigin: false },
            "/clip": { target: "http://127.0.0.1:8080", changeOrigin: false },
          }
        : undefined,
    },

    test: {
      environment: "node",
      // Loads .env.test.local into process.env for real-LLM tests.
      // The loader itself is a no-op if the file is absent, so this is
      // safe to keep on for every test run.
      setupFiles: ["./src/test-helpers/load-test-env.ts"],
    },
  }
})
