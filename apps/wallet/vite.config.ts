import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import { fileURLToPath } from "node:url";

// Tauri expects a fixed port and no clearing of its logs.
const host = process.env.TAURI_DEV_HOST;

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  resolve: {
    alias: {
      // See src/hw/bs58check-shim.ts.
      bs58check: fileURLToPath(new URL("./src/hw/bs58check-shim.ts", import.meta.url)),
    },
  },
  server: {
    port: 1420,
    strictPort: true,
    host: host || false,
    hmr: host ? { protocol: "ws", host, port: 1421 } : undefined,
    watch: { ignored: ["**/src-tauri/**"] },
  },
  envPrefix: ["VITE_", "TAURI_ENV_*"],
  define: {
    // True in `tauri build` / `tauri dev`: the app never ships the mock API.
    __APP_BUILD__: JSON.stringify(Boolean(process.env.TAURI_ENV_PLATFORM)),
  },
  build: {
    target: "es2022",
    sourcemap: false,
  },
});
