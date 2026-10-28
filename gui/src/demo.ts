/**
 * Made-up windows and elements for opening the interface in a plain browser
 * (`npm run dev`) without the desktop backend. Never used inside the app.
 */
import type { ElementNode, Resource, Step } from "./api";

const windows: Resource[] = [
  {
    resource_id: "demo:notes", provider: "desktop.windows_uia", kind: "desktop",
    name: "便签 - 记事本", capabilities: [], metadata: { process_name: "notepad.exe", window_id: "hwnd:1" },
    app: { provider: "desktop.windows_uia", process: "notepad.exe" },
  },
  {
    resource_id: "demo:settings", provider: "desktop.windows_uia", kind: "desktop",
    name: "设置", capabilities: [], metadata: { process_name: "SystemSettings.exe" },
    app: { provider: "desktop.windows_uia", process: "SystemSettings.exe" },
  },
  {
    resource_id: "demo:shop", provider: "browser.cdp", kind: "browser",
    name: "示例商城 · 退款", capabilities: [], metadata: { url: "https://shop.example.com/refund" },
    app: { provider: "browser.cdp", url: "https://shop.example.com/refund" },
  },
];

const element = (id: string, role: string, name: string, capabilities: string[], extra: Partial<ElementNode> = {}): ElementNode => ({
  node_id: id, role, name, capabilities, step_locator: { role, name }, ...extra,
});

const elements: ElementNode[] = [
  element("n1", "Document", "文本编辑器", ["focus", "fill", "type_text", "key", "click", "scroll"], { value: "今天要做的事" }),
  element("n2", "MenuItem", "文件", ["invoke", "click", "hover"]),
  element("n3", "MenuItem", "编辑", ["invoke", "click", "hover"]),
  element("n4", "Button", "保存", ["invoke", "click", "drag"]),
  element("n5", "ComboBox", "编码", ["select", "focus"], { options: ["UTF-8", "GBK", "UTF-16"] }),
  element("n6", "CheckBox", "自动换行", ["check", "click"]),
  element("n7", "Text", "第 1 行，第 7 列", []),
  element("n8", "List", "最近打开", ["scroll", "drag"]),
];

export async function demo<T>(command: string, args: Record<string, unknown> = {}): Promise<T> {
  await new Promise((resolve) => setTimeout(resolve, 150));
  const answer = ((): unknown => {
    switch (command) {
      case "list_targets":
      case "connect_browser":
        return { resources: windows, unavailable: {}, recording_supported: true, browser_port: null };
      case "observe":
        if ((args.view as { mode?: string }).mode === "overview") {
          return { regions: [{ name: "菜单栏" }, { name: "编辑区" }, { name: "状态栏" }], node_count: 8, app: {} };
        }
        return {
          nodes: elements, node_count: elements.length, matched: elements.length,
          app: windows.find((w) => w.resource_id === args.resourceId)?.app ?? {},
        };
      case "try_locator":
        return { found: true, match_count: 1, node: elements[3] };
      case "perform_step":
        return { step: (args.step as Step).id };
      case "run_steps":
        return {
          status: "succeeded",
          steps: (args.steps as Step[]).map((step) => ({ id: step.id, status: "succeeded" })),
        };
      case "collect_recording":
      case "stop_recording":
        return { steps: [], skipped: [], recording: command === "collect_recording" };
      case "save_recording":
        return { recording_path: "~/recordings/demo.recording.json", workflow_path: "" };
      case "list_recordings":
        return { recordings: [], directory: "~/recordings" };
      default:
        return {};
    }
  })();
  return answer as T;
}
