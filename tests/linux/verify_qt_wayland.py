"""Run one Qt ABI's public-CLI effects inside a fresh virtual KWin and session bus."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from wayland_form import wait_for


def verify(aad, fixture, plugin_root):
    def call(*args, success=True):
        result = subprocess.run([aad, *args], capture_output=True, text=True, timeout=30)
        assert (result.returncode == 0) == success, (args, result.stdout, result.stderr)
        return json.loads(result.stdout) if success else result

    with tempfile.TemporaryDirectory() as directory:
        root = Path(directory)
        os.environ.update(XDG_RUNTIME_DIR=directory, XDG_CONFIG_HOME=str(root / "config"),
                          XDG_DATA_HOME=str(root / "data"), XDG_CACHE_HOME=str(root / "cache"),
                          XDG_SESSION_TYPE="wayland", XDG_CURRENT_DESKTOP="KDE", WAYLAND_DISPLAY="aad-qt")
        for name in ("DISPLAY", "AT_SPI_BUS_ADDRESS", "NO_AT_BRIDGE", "WAYLAND_SOCKET"):
            os.environ.pop(name, None)
        with subprocess.Popen(["kwin_wayland", "--virtual", "--no-lockscreen", "--socket", "aad-qt"]) as server:
            try:
                wait_for(lambda: (root / "aad-qt").is_socket(), "private compositor")
                plugins = os.pathsep.join(filter(None, [plugin_root, os.environ.get("QT_PLUGIN_PATH")]))
                env = dict(os.environ, QT_QPA_PLATFORM="wayland", QT_LINUX_ACCESSIBILITY_ALWAYS_ON="1",
                           QT_PLUGIN_PATH=plugins, QT_QPA_GENERIC_PLUGINS="aad-wayland-input")
                log = root / "receipt"
                with log.open("w") as stream, subprocess.Popen([fixture], env=env, stdout=stream) as child:
                    try:
                        def emitted(line):
                            return line in log.read_text().splitlines()
                        rid = wait_for(lambda: next((app["resource_id"] for app in call("apps")["apps"]
                            if app["metadata"].get("process_id") == child.pid), None), "AT-SPI registration")
                        wait_for(lambda: any(node.get("name") == "Invoice source" and "key" in node["capabilities"]
                            for node in call("snapshot", rid)["nodes"]), "directed input capability")
                        call("do", "key", rid, "--name", "Invoice source", "--keys", "home")
                        before = len(log.read_text().splitlines())
                        call("do", "key", rid, "--name", "Invoice source", "--keys", "ctrl,a")
                        wait_for(lambda: "SELECTION selected invoice 2037" in log.read_text().splitlines()[before:],
                                 "selected source text")
                        def drag(target, *, end=".5", success=True):
                            return call("do", "drag", rid, "--name", "Invoice source", "--to-name", target,
                                        "--fx", ".15", "--fy", ".5", "--to-fx", end, "--to-fy", ".5", success=success)
                        initial = log.read_text()
                        drag("Same window receiver", end="0", success=False)
                        assert log.read_text() == initial, "covered endpoint changed the UI"
                        drag("Same window receiver")
                        wait_for(lambda: emitted("LOCAL selected invoice 2037"), "same-window dropped text")
                        drag("Other window receiver")
                        wait_for(lambda: emitted("REMOTE selected invoice 2037"), "other-window dropped text")
                        failure = drag("Refusing receiver", success=False)
                        assert json.loads(failure.stdout)["error"]["effect"] == "unknown", failure.stdout
                        assert emitted("REFUSED") and not emitted("WRONG RECEIVER"), log.read_text()
                        call("do", "click", rid, "--name", "Other window receiver", success=False)
                        call("do", "click", rid, "--name", "Submit invoice")
                        wait_for(lambda: emitted("CLICK"), "button activation")
                        call("do", "scroll", rid, "--name", "Invoice scroll", "--direction", "down")
                        wait_for(lambda: any(line.startswith("WHEEL ") and int(line[6:]) != 80
                            for line in log.read_text().splitlines()), "scroll value change")
                        print(log.read_text(), flush=True)
                    finally:
                        child.terminate()
            finally:
                server.terminate()


if __name__ == "__main__":
    if sys.argv[1:2] == ["--private-bus"]:
        verify(*sys.argv[2:])
    else:
        sys.exit(subprocess.call(["dbus-run-session", "--", sys.executable, __file__, "--private-bus", *sys.argv[1:]]))
