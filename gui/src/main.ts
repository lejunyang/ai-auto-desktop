import { createApp } from "vue";
import App from "./App.vue";
import { useTransport } from "./api";
import "./styles.css";

async function start(): Promise<void> {
  // Outside the desktop app there is no backend: show made-up data instead.
  if (!("__TAURI_INTERNALS__" in window)) {
    const { demo } = await import("./demo");
    useTransport(demo);
  }
  createApp(App).mount("#app");
}

void start();
