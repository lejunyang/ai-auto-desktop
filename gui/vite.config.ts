import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// Tauri serves the built assets from `dist` and the dev server on a fixed port,
// so the port must not drift: the desktop shell points at exactly this one.
export default defineConfig({
  plugins: [vue()],
  clearScreen: false,
  server: {
    port: 5173,
    strictPort: true,
  },
  build: {
    // Match the WebView2 / WKWebView baselines Tauri targets.
    target: "es2021",
    outDir: "dist",
    emptyOutDir: true,
  },
  test: {
    environment: "happy-dom",
    include: ["src/**/*.test.ts"],
  },
});
