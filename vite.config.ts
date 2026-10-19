import { defineConfig } from "vite";

// One build serves both the desktop app (Tauri) and the GitHub Pages site,
// so all asset paths are relative.
export default defineConfig({
  root: "ui",
  base: "./",
  clearScreen: false,
  server: { port: 5173, strictPort: true },
  build: { outDir: "../dist", emptyOutDir: true, target: "es2022" },
});
