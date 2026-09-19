import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Desktop-only dev server. The port is fixed and strict because Tauri's
// `devUrl` in tauri.conf.json points at exactly this address: silently moving to
// another port would leave the GUI showing a blank window.
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    watch: {
      // Rust sources are rebuilt by cargo, not watched by Vite.
      ignored: ["**/src-tauri/**"],
    },
  },
  build: {
    // Tauri uses WebKitGTK on Linux (and WebKit/Chromium elsewhere); ES2021 is
    // comfortably within what the reference machine's WebKit 2.52 supports.
    target: "es2021",
    sourcemap: true,
  },
});
