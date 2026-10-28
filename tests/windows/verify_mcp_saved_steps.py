#!/usr/bin/env python3
"""Save control steps over MCP with save_workflow, then replay them with run_workflow.

An isolated Win32 probe window is created and destroyed here. An MCP client
(this script, standing in for an agent) observes the window, saves a fill and a
check by role and name, clears the edit box, and runs the saved workflow by
name. The edit box is read back independently with WM_GETTEXT.
"""
import ctypes
import json
import os
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
REPO = os.path.dirname(os.path.dirname(HERE))
AAD = os.path.join(REPO, "target", "debug", "aad.exe")
PROBE = os.path.join(HERE, "input_channel_probe.py")
WM_GETTEXT = 0x000D


def edit_text(handle: int) -> str:
    buffer = ctypes.create_unicode_buffer(256)
    ctypes.windll.user32.SendMessageW(handle, WM_GETTEXT, 256, buffer)
    return buffer.value


class Client:
    def __init__(self, env: dict):
        self.process = subprocess.Popen([AAD, "mcp", "--allow-input"], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, text=True, encoding="utf-8",
                                        env=env)
        self.next = 0
        self.request("initialize", {"protocolVersion": "2025-06-18", "capabilities": {},
                                    "clientInfo": {"name": "probe", "version": "0"}})
        self.send({"jsonrpc": "2.0", "method": "notifications/initialized"})

    def send(self, message: dict) -> None:
        self.process.stdin.write(json.dumps(message) + "\n")
        self.process.stdin.flush()

    def request(self, method: str, params: dict) -> dict:
        self.next += 1
        self.send({"jsonrpc": "2.0", "id": self.next, "method": method, "params": params})
        while True:
            answer = json.loads(self.process.stdout.readline())
            if answer.get("id") == self.next:
                return answer

    def tool(self, name: str, arguments: dict) -> tuple[dict, bool]:
        result = self.request("tools/call", {"name": name, "arguments": arguments})["result"]
        return json.loads(result["content"][0]["text"]), bool(result.get("isError"))

    def close(self) -> None:
        self.process.stdin.close()
        self.process.wait(timeout=30)


def main() -> int:
    store = tempfile.mkdtemp(prefix="aad-mcp-store-")
    env = dict(os.environ, AAD_RECORDINGS_DIR=store)
    title = f"AAD MCP Steps Probe {os.getpid()}"
    log = os.path.join(tempfile.gettempdir(), f"aad-mcp-steps-{os.getpid()}.jsonl")
    probe = subprocess.Popen([sys.executable, PROBE, "--title", title, "--log", log],
                             stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
    client = None
    try:
        ready = json.loads(probe.stdout.readline())
        client = Client(env)
        targets, _ = client.tool("control_targets", {})
        mine = [r for r in targets["data"]["resources"] if r["name"] == title]
        observed, _ = client.tool("control_observe", {
            "resource_id": mine[0]["resource_id"],
            "view": {"mode": "outline"}})
        # The outline carries a durable locator per element: that is what gets saved.
        edit = next(n for n in observed["data"]["nodes"] if n["locator"].get("role") == "edit")
        app = {"provider": mine[0]["provider"], "title": title}
        saved, refused = client.tool("save_workflow", {
            "name": "mcp-probe",
            "steps": [
                {"app": app, "locator": edit["locator"],
                 "action": {"type": "fill", "value": "saved by an agent"}},
                {"app": app, "locator": {"role": "button", "name": "Probe target"},
                 "check": {"present": True, "within_ms": 3000}},
            ],
        })
        described, _ = client.tool("describe_workflow", {"name": "mcp-probe"})

        ctypes.windll.user32.SendMessageW(ready["edit_handle"], 0x000C, 0, "")  # WM_SETTEXT
        cleared = edit_text(ready["edit_handle"])
        result, failed = client.tool("run_workflow", {"name": "mcp-probe"})
        replayed = edit_text(ready["edit_handle"])
        findings = {
            "saved": not refused and saved.get("kind") == "WorkflowSaved",
            "save_answer": saved if refused else None,
            "described": [a.get("action") for a in described.get("actions", [])],
            "cleared_before_replay": cleared == "",
            "run_status": result.get("status"),
            "run_error": result if failed else None,
            "replayed_text": replayed,
        }
        findings["passed"] = (findings["saved"] and findings["cleared_before_replay"]
                              and not failed and replayed == "saved by an agent")
        print(json.dumps(findings, ensure_ascii=False, indent=1))
        return 0 if findings["passed"] else 1
    finally:
        if client:
            client.close()
        probe.terminate()


if __name__ == "__main__":
    raise SystemExit(main())
