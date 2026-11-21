"""Run in a private Wayland + D-Bus session, with DISPLAY unset."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


def application(receipt):
    import gi
    gi.require_version("Gtk", "3.0")
    gi.require_version("Gdk", "3.0")
    from gi.repository import Gdk, GLib, Gtk

    GLib.set_prgname("aad-wayland-expense")
    GLib.set_application_name("AAD Wayland Expense")
    window = Gtk.Window(title="AAD Wayland Expense")
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
    field = Gtk.Entry()
    field.get_accessible().set_name("Invoice number")
    button = Gtk.Button(label="Save invoice")
    def save(_button):
        result = {"number": field.get_text(), "display": Gdk.Display.get_default().__gtype__.name}
        Path(receipt).write_text(json.dumps(result), encoding="utf-8")
    button.connect("clicked", save)
    box.add(field)
    box.add(button)
    window.add(box)
    window.show_all()
    Gtk.main()


def wait_for(get_value, description):
    deadline = time.monotonic() + 15
    while time.monotonic() < deadline:
        value = get_value()
        if value:
            return value
        time.sleep(0.15)
    raise RuntimeError("Timed out waiting for " + description)


def verify(aad):
    if os.environ.get("DISPLAY") or not os.environ.get("WAYLAND_DISPLAY"):
        raise RuntimeError("Use a private native Wayland session without DISPLAY")

    def call(*args):
        result = subprocess.run([aad, *args], capture_output=True, text=True, timeout=40)
        if result.returncode:
            raise RuntimeError(result.stdout + result.stderr)
        return json.loads(result.stdout)

    with tempfile.TemporaryDirectory() as root:
        receipt = Path(root) / "receipt.json"
        child = subprocess.Popen([sys.executable, __file__, "--app", str(receipt)])
        try:
            def locate():
                resources = call("apps")["apps"]
                return next((r["resource_id"] for r in resources
                             if r["provider"] == "desktop.linux_atspi" and
                             r["metadata"].get("process_id") == child.pid), None)
            resource = wait_for(locate, "Wayland application")
            call("do", "fill", resource, "--name", "Invoice number", "--value", "INV-2037")
            call("do", "invoke", resource, "--name", "Save invoice")
            wait_for(receipt.is_file, "receipt from GTK")
            result = json.loads(receipt.read_text(encoding="utf-8"))
            assert result == {"number": "INV-2037", "display": "GdkWaylandDisplay"}, result
            print(json.dumps(result))
        finally:
            child.terminate()
            child.wait(timeout=8)


if __name__ == "__main__":
    if sys.argv[1] == "--app":
        application(sys.argv[2])
    else:
        verify(sys.argv[1])
