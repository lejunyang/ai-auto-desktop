"""Native AT-SPI activation on Electron in a private, serial KWin session."""
import argparse
from contextlib import contextmanager
import json
import os
from pathlib import Path
import select
import shutil
import signal
import subprocess
import sys
import tempfile
import time


def wait_for(read, description, child):
    until = time.monotonic() + 20
    while time.monotonic() < until:
        if child.poll() is not None:
            raise RuntimeError(f"{description}: process exited {child.returncode}")
        value = read()
        if value:
            return value
        time.sleep(.1)
    raise TimeoutError(description)


@contextmanager
def process(args, **kwargs):
    child = subprocess.Popen(args, start_new_session=True, **kwargs)
    try:
        yield child
    finally:
        if child.poll() is None:
            os.killpg(child.pid, signal.SIGTERM)
            try:
                child.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(child.pid, signal.SIGKILL)
                child.wait()


class Control:
    def __init__(self, child):
        self.child = child
        self.session = None
        self.sequence = 0
        self.call("capabilities")

    def call(self, method, *, expect_error=None, **fields):
        self.sequence += 1
        request = dict(request_id=str(self.sequence), session_id=self.session, timeout_ms=10000,
                       operation=dict(method=method, **fields))
        print(json.dumps(request), file=self.child.stdin, flush=True)
        if not select.select([self.child.stdout], [], [], 15)[0]:
            raise TimeoutError(method)
        reply = json.loads(self.child.stdout.readline())
        self.session = reply["session_id"]
        error = reply.get("error")
        if expect_error:
            assert error and error["code"] == expect_error, reply
            return error
        assert not error, reply
        return reply["data"]

    def node(self, resource, label, role, verb):
        view = self.call("observe", resource_id=resource, view={"mode": "nodes"})
        assert not view["truncated"], view
        matches = [n for n in view["nodes"] if n.get("name") == label and n["role"] == role]
        assert len(matches) == 1, [(n["role"], n.get("name")) for n in view["nodes"]]
        node = matches[0]
        provenance = node["provenance"]
        assert provenance["toolkit_name"] == "Chromium", provenance
        assert provenance["native_invoke_name"] == verb, provenance["atspi_actions"]
        assert "invoke" in node["capabilities"], node
        return node

    def invoke(self, node, verb):
        result = self.call("act", target=node["target"], action={"type": "invoke"})
        assert result["provider"] == "desktop.linux_atspi", result
        receipt = result["backend_result"]["backend_result"]
        assert receipt["native_interface"] == "Action.do_action" and receipt["dispatched"], receipt
        assert receipt["native_action_name"] == verb, receipt


def make_runtime(code, root):
    # Electron resolves resources/app next to the actual executable. Give the
    # fixture its own executable and resources. Share only immutable runtime files.
    runtime = root / "runtime"
    runtime.mkdir()
    binary = runtime / code.name
    shutil.copyfile(code, binary)
    binary.chmod(0o755)
    for item in code.parent.iterdir():
        if item.name not in (code.name, "resources", "bin"):
            (runtime / item.name).symlink_to(item)
    shutil.copytree(Path(__file__).with_name("electron_atspi_fixture"), runtime / "resources/app")
    return binary


def exercise(args, app, receipt):
    def state():
        return json.loads(receipt.read_text())

    def expect(fragment, checked, changes):
        def read():
            current = state()
            return current if (current["hash"], current["approved"], current["other"], current["changes"]) == (
                fragment, checked, False, [["approved", value] for value in changes]) else None
        return wait_for(read, f"renderer hash={fragment!r}, checked={checked}, changes={changes}", app)

    wait_for(receipt.is_file, "initial Electron renderer receipt", app)
    initial = expect("", False, [])
    command = [args.aad, "control", "serve", "--allow-input"]
    stdio = dict(stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    with process(command, **stdio) as first, process(command, **stdio) as second:
        client, observer = Control(first), Control(second)

        def resource_for(client):
            resources = client.call("targets")["resources"]
            matches = [r["resource_id"] for r in resources if r["provider"] == "desktop.linux_atspi"
                       and r["metadata"].get("process_id") == app.pid]
            assert len(matches) <= 1, matches
            return matches[0] if matches else None

        resource = wait_for(lambda: resource_for(client), "fixture AT-SPI application", app)
        assert resource_for(observer) == resource
        link = client.node(resource, "Open invoice details", "link", "jump")
        client.invoke(link, "jump")
        expect("#details", False, [])
        stale = observer.node(resource, "Approve invoice", "check_box", "check")
        unchecked = client.node(resource, "Approve invoice", "check_box", "check")
        client.invoke(unchecked, "check")
        expect("#details", True, [True])
        # Release the public control input lease without disturbing the
        # independent session that observed the original unchecked object.
        first.stdin.close()
        assert first.wait(timeout=10) == 0
        observer.call("act", target=stale["target"], action={"type": "invoke"},
                      expect_error="DRIVER.STALE_SNAPSHOT")
        checked = observer.node(resource, "Approve invoice", "check_box", "uncheck")
        for key in ("bus_name", "object_path"):
            assert checked["provenance"][key] == unchecked["provenance"][key]
        expect("#details", True, [True])
        observer.invoke(checked, "uncheck")
        final = expect("#details", False, [True, False])
        print(json.dumps(dict(electron=initial["electron"], provider="desktop.linux_atspi",
                              verbs=["jump", "check", "uncheck"], stale="DRIVER.STALE_SNAPSHOT",
                              hash=final["hash"], changes=final["changes"], sibling=final["other"])))


def verify(args, root):
    binary = make_runtime(args.code, root)
    receipt = root / "state.json"
    with process(["kwin_wayland", "--virtual", "--no-lockscreen", "--socket", "aad-atspi"]) as kwin:
        wait_for(lambda: (root / "aad-atspi").is_socket(), "private Wayland socket", kwin)
        with process([binary, "--ozone-platform=wayland", "--force-renderer-accessibility",
                      "--disable-gpu", "--password-store=basic", "--use-inmemory-secretstorage",
                      "--user-data-dir=" + str(root / "profile"), "--shared-data-dir=" + str(root / "shared")],
                     env=os.environ | {"AAD_FIXTURE_RECEIPT": str(receipt)}) as app:
            exercise(args, app, receipt)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--aad", type=Path, default=Path("target/debug/aad"))
    parser.add_argument("--code", type=Path, default=Path("/usr/share/code/code"))
    parser.add_argument("--private-session", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.private_session:
        assert not os.getenv("DISPLAY") and os.environ["WAYLAND_DISPLAY"] == "aad-atspi"
        verify(args, Path(os.environ["XDG_RUNTIME_DIR"]))
    else:
        with tempfile.TemporaryDirectory(prefix="aad-electron-atspi-") as tmp:
            env = dict(os.environ, XDG_RUNTIME_DIR=tmp, XDG_SESSION_TYPE="wayland",
                       WAYLAND_DISPLAY="aad-atspi", GDK_BACKEND="wayland", LC_ALL="C.UTF-8",
                       XDG_CONFIG_HOME=tmp+"/config", XDG_DATA_HOME=tmp+"/data",
                       XDG_CACHE_HOME=tmp+"/cache", AAD_CONTROL_LEASE_DIR=tmp+"/leases")
            for name in ("DISPLAY", "WAYLAND_SOCKET", "AT_SPI_BUS_ADDRESS", "NO_AT_BRIDGE",
                         "VSCODE_IPC_HOOK_CLI", "ELECTRON_RUN_AS_NODE"):
                env.pop(name, None)
            try:
                subprocess.run(["dbus-run-session", "--", sys.executable, __file__, "--private-session",
                                "--code", args.code.resolve(), "--aad", args.aad.resolve()], env=env, check=True)
            finally:
                # The portal unmounts its private document filesystem
                # asynchronously after the session bus exits.
                deadline = time.monotonic() + 10
                while os.path.ismount(Path(tmp) / "doc") and time.monotonic() < deadline:
                    time.sleep(.1)
