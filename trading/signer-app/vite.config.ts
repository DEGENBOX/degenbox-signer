import { fileURLToPath } from "node:url";
import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Vite ports are deterministic so `tauri.conf.json` can hardcode them.
// Tauri reserves :1430-:1431; we use :5174 to leave room for the main
// webapp (`:5173`) if both run in parallel during dev.
//
// fs.allow: signer-app is a member of the frontend pnpm workspace, but
// frontend/ is NOT an ancestor directory — Vite's workspace-root
// detection stops at this package, so /@fs/ requests for files that
// resolve into frontend/node_modules (e.g. @fontsource woff2s) 403 in
// dev without widening the allow list to the repo root.
const repoRoot = fileURLToPath(new URL("../..", import.meta.url));

export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 5174,
    strictPort: true,
    host: "127.0.0.1",
    fs: {
      allow: [repoRoot],
    },
  },
  envPrefix: ["VITE_", "TAURI_"],
  build: {
    target: "es2022",
    minify: "esbuild",
    sourcemap: false,
  },
});
