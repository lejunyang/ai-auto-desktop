import { flushPromises, mount } from "@vue/test-utils";
import { beforeEach, expect, it, vi } from "vitest";
import App from "./App.vue";
import { useTransport, type Resource, type Step } from "./api";
import RepairStep from "./components/RepairStep.vue";
import { readEvidence } from "./recordingEvidence";

const app = { provider: "browser.cdp", url: "https://example.test/orders" };
const resource: Resource = { resource_id: "page", app, kind: "browser", provider: app.provider, name: "Orders", capabilities: [], metadata: { url: app.url } };
const steps: Step[] = ["Start", "Save", "Receipt"].map((name, i) => ({
  id: "rec" + (i + 1), app, locator: { role: "button", name }, action: { type: "click" },
}));
const node = { node_id: "5", role: "button", name: "Save draft", capabilities: ["click"], step_locator: { role: "button", name: "Save draft" } };

beforeEach(() => { vi.restoreAllMocks(); });

it("only accepts images in the selected recording and indexes by the original row ID", async () => {
  const file = (name: string, contents: string) => {
    const f = new File([contents], name.split("/").pop()!);
    Object.defineProperty(f, "webkitRelativePath", { value: name });
    return f;
  };
  const png = "images/" + "a".repeat(64) + ".png";
  const picture = file("capture/" + png, "png");
  const rows = file("capture/steps.jsonl", JSON.stringify({ id: 7, screenshots: [{ path: png }, { path: "../private.png" }] }));
  const archive = await readEvidence([rows, picture, file("capture/network.jsonl", "ignored")]);
  expect(archive.screenshots.get(7)).toEqual([picture]);
});

it("replaces only after a unique match and discards the preview on a new selection", async () => {
  useTransport(async <T>(command: string) => (command === "try_locator" ? { found: true, match_count: 1 } : { data: "png" }) as T);
  const wrapper = mount(RepairStep, { props: { step: steps[1], shots: [], resource, node, busy: false } });
  const buttons = wrapper.findAll("footer button");
  expect(buttons[1].attributes("disabled")).toBeDefined();
  await buttons[0].trigger("click");
  await flushPromises();
  expect(wrapper.get("img").attributes("src")).toBe("data:image/png;base64,png");
  await buttons[1].trigger("click");
  expect(wrapper.emitted("replace")?.[0]).toEqual([{ ...steps[1], locator: node.step_locator }]);
  await wrapper.setProps({ node: { ...node, name: "Other", step_locator: { name: "Other" } } });
  expect(buttons[1].attributes("disabled")).toBeDefined();
  expect(wrapper.find("img").exists()).toBe(false);
  await wrapper.setProps({ node: { ...node, capabilities: ["focus"] } });
  await buttons[0].trigger("click");
  await flushPromises();
  expect(buttons[1].attributes("disabled")).toBeDefined();
  expect(wrapper.text()).toContain("当前元素未提供这一步需要的操作");
  wrapper.unmount();
});

it("runs the selected suffix without replaying earlier writes", async () => {
  let executed: Step[] = [];
  useTransport(async <T>(command: string, args?: Record<string, unknown>) => {
    switch (command) {
      case "list_targets": return { resources: [], unavailable: {}, recording_supported: false } as T;
      case "list_recordings": return { directory: "saved", recordings: [{ name: "Orders", path: "orders" }] } as T;
      case "open_recording": return { name: "Orders", steps } as T;
      case "run_steps":
        executed = args!.steps as Step[];
        return { status: "succeeded", steps: executed.map(s => ({ id: s.id, status: "succeeded" })) } as T;
      default: throw Error(command);
    }
  });
  const wrapper = mount(App, { attachTo: document.body });
  const click = async (text: string) => {
    const button = [...document.querySelectorAll("button")].find(b => b.textContent?.trim() === text)!;
    button.click();
    await flushPromises();
  };
  await click("打开…");
  document.querySelector<HTMLButtonElement>("button.saved")!.click();
  await flushPromises();
  await wrapper.findAll("button[title='编辑']")[1].trigger("click");
  await click("从这一步试跑");
  await click("运行这 2 步");
  expect(executed.map(s => s.id)).toEqual(["rec2", "rec3"]);
  wrapper.unmount();
});
