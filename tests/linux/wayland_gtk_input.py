"""Effects in GTK's own widget state on a private native Wayland display."""
import json
import os
from pathlib import Path
import subprocess
import sys
import time

from wayland_form import wait_for


def application(major, receipt):
    import gi
    gi.require_version("Gtk", major + ".0")
    gi.require_version("Gdk", major + ".0")
    from gi.repository import Gdk, GLib, Gtk

    GLib.set_prgname("aad-wayland-gtk" + major)
    loop = GLib.MainLoop()
    window = Gtk.Window(title="Invoice form")
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
    clicks = []
    if major == "3":
        entry = Gtk.Entry()
        entry.set_text("INV-2037")
        entry.get_accessible().set_name("Invoice")
        sibling = Gtk.Entry()
        sibling.get_accessible().set_name("Other invoice")
        phases = []

        def key_phase(widget, event, phase):
            key = Gdk.keyval_name(event.keyval)
            phases.append([widget.get_accessible().get_name(), phase, key])
            if phase == "press" and key == "z":
                sibling.grab_focus()
                return True
            return False

        for widget in (entry, sibling):
            widget.connect("key-press-event", key_phase, "press")
            widget.connect("key-release-event", key_phase, "release")
        secret = Gtk.Entry()
        secret.set_visibility(False)
        secret.get_accessible().set_name("PIN")
        focus_pin = Gtk.Button(label="Focus PIN")
        focus_pin.connect("clicked", lambda *_: secret.grab_focus())
        for widget in [entry, sibling, secret, focus_pin]:
            box.add(widget)
        scale = Gtk.Scale.new_with_range(Gtk.Orientation.HORIZONTAL, 0, 100, 1)
        scale.set_size_request(250, 45)
        scale.set_value(50)
        scale.get_accessible().set_name("Quantity")
        wheel = []

        def on_scroll(_widget, event):
            wheel.append(event.direction.value_nick)
            return False

        scale.connect("scroll-event", on_scroll)
        overlay = Gtk.Overlay()
        overlay.add(scale)
        cover = Gtk.EventBox()
        cover.set_size_request(35, -1)
        cover.set_halign(Gtk.Align.START)
        overlay.add_overlay(cover)
        box.add(overlay)
        window.add(box)
        window.show_all()
        entry.grab_focus()
        # GtkEntry can select its contents on focus. Establish the initial
        # caret after the window has received focus before testing Ctrl+A.
        prepared = False

        def prepare():
            nonlocal prepared
            if not entry.has_focus():
                return True
            entry.select_region(0, 0)
            prepared = True
            return False

        GLib.timeout_add(25, prepare)

        def state():
            selection = entry.get_selection_bounds()
            return dict(text=entry.get_text(), selected=list(selection),
                        sibling=sibling.get_text(), secret=bool(secret.get_text()),
                        prepared=prepared, pin_focused=secret.has_focus(),
                        wheel=wheel, quantity=scale.get_value(), phases=phases)
    else:
        editor = Gtk.TextView()
        editor.update_property([Gtk.AccessibleProperty.LABEL], ["Invoice notes"])
        button = Gtk.Button(label="Save invoice")
        button.update_property([Gtk.AccessibleProperty.LABEL], ["Submit form"])
        button.connect("clicked", lambda *_: clicks.append("saved"))
        disabled = Gtk.Button(label="Disabled action")
        disabled.update_property([Gtk.AccessibleProperty.LABEL], ["Unavailable submission"])
        disabled.set_sensitive(False)
        disabled.connect("clicked", lambda *_: clicks.append("disabled"))
        hidden = Gtk.Entry()
        hidden.set_visibility(False)
        hidden.update_property([Gtk.AccessibleProperty.LABEL], ["Hidden entry"])
        for widget in [editor, button, disabled, hidden]:
            box.append(widget)
        window.set_child(box)
        window.present()

        def state():
            buffer = editor.get_buffer()
            return dict(text=buffer.get_text(*buffer.get_bounds(), False), clicks=clicks,
                        hidden_changed=bool(hidden.get_text()))

    def publish():
        result = dict(display=Gdk.Display.get_default().__gtype__.name, **state())
        temp = receipt.with_suffix(".tmp")
        temp.write_text(json.dumps(result))
        temp.replace(receipt)
        return True

    GLib.timeout_add(40, publish)
    loop.run()


def run(aad, module, root, major, enabled):
    receipt = root / ("receipt-" + major + str(enabled) + ".json")
    env = os.environ.copy()
    if enabled and major == "3":
        env["GTK_MODULES"] = module
    child = subprocess.Popen([sys.executable, __file__, "--app", major, str(receipt)], env=env)

    def call(*args, success=True):
        result = subprocess.run([aad, *args], capture_output=True, text=True, timeout=40)
        if success != (result.returncode == 0):
            raise AssertionError((args, result.stdout, result.stderr))
        return json.loads(result.stdout) if success else None

    def state():
        return json.loads(receipt.read_text())

    try:
        wait_for(receipt.is_file, "GTK " + major)
        assert state()["display"] == "GdkWaylandDisplay", state()
        resource = wait_for(lambda: next((r["resource_id"] for r in call("apps")["apps"]
                            if r["provider"] == "desktop.linux_atspi"
                            and r["metadata"].get("process_id") == child.pid), None), "AT-SPI resource")
        if major == "3":
            wait_for(lambda: state()["prepared"] and not state()["selected"], "collapsed GTK caret")
            if enabled:
                nodes = call("snapshot", resource)["nodes"]
                quantity = next(n for n in nodes if n.get("name") == "Quantity")
                assert "scroll" in quantity["capabilities"] and "hover" not in quantity["capabilities"], quantity
                call("do", "key", resource, "--name", "Invoice", "--keys", "ctrl,a")
                wait_for(lambda: state()["selected"] == [0, 8], "GTK selection")
                call("do", "key", resource, "--name", "Invoice", "--keys", "k")
                wait_for(lambda: state()["text"] == "k", "GTK replacement")
                for direction in ["up", "down", "left", "right"]:
                    before = state()
                    call("do", "scroll", resource, "--name", "Quantity", "--direction", direction,
                         "--notches", "2", "--fx", ".7", "--fy", ".5")
                    wait_for(lambda: state()["wheel"] == before["wheel"] + [direction] * 2,
                             "GTK " + direction + " scroll events")
                    assert state()["quantity"] != before["quantity"], (direction, state())
                    print("GtkScale", direction, before["quantity"], "->", state()["quantity"], flush=True)
                before = state()
                call("do", "scroll", resource, "--name", "Quantity", "--direction", "up",
                     "--fx", ".02", "--fy", ".5", success=False)
                time.sleep(.1)
                assert state() == before
            else:
                call("do", "key", resource, "--name", "Invoice", "--keys", "k", success=False)
                call("do", "scroll", resource, "--name", "Quantity", "--direction", "up", success=False)
                assert not state()["wheel"] and state()["quantity"] == 50
            for name in ("Other invoice", "PIN"):
                call("do", "key", resource, "--name", name, "--keys", "q", success=False)
            if enabled:
                call("do", "key", resource, "--name", "Invoice", "--keys", "z", success=False)
                wait_for(lambda: state()["phases"][-1:] == [["Invoice", "press", "z"]],
                         "focus transfer during press")
                assert state()["phases"] == [["Invoice", phase, key] for key in ("a", "k")
                                             for phase in ("press", "release")] + [["Invoice", "press", "z"]]
            call("do", "invoke", resource, "--name", "Focus PIN")
            wait_for(lambda: state()["pin_focused"], "focused password entry")
            call("do", "key", resource, "--name", "PIN", "--keys", "q", success=False)
            time.sleep(.1)
            assert not state()["sibling"] and not state()["secret"]
            assert state()["text"] == ("k" if enabled else "INV-2037")
        else:
            call("do", "fill", resource, "--name", "Invoice notes", "--value", "発票 2037")
            wait_for(lambda: state()["text"] == "発票 2037", "GTK 4 text buffer")
            call("do", "invoke", resource, "--name", "Submit form")
            wait_for(lambda: state()["clicks"] == ["saved"], "GTK 4 clicked handler")
            call("do", "invoke", resource, "--name", "Unavailable submission", success=False)
            call("do", "fill", resource, "--name", "Hidden entry", "--value", "refused", success=False)
            time.sleep(.1)
            assert state()["clicks"] == ["saved"] and not state()["hidden_changed"]
        print(json.dumps(dict(gtk=major, module=enabled, **state()), ensure_ascii=False), flush=True)
    finally:
        child.terminate()
        child.wait(timeout=10)


if __name__ == "__main__":
    if sys.argv[1] == "--app":
        application(sys.argv[2], Path(sys.argv[3]))
    else:
        root = Path(os.environ["XDG_RUNTIME_DIR"])
        assert not os.getenv("DISPLAY") and os.environ["WAYLAND_DISPLAY"] == "aad-gtk"
        compositor = subprocess.Popen(["kwin_wayland", "--virtual", "--no-lockscreen",
                                       "--socket", "aad-gtk"], stdout=subprocess.DEVNULL)
        try:
            wait_for(lambda: (root / "aad-gtk").is_socket(), "private KWin")
            for major, enabled in [("4", False), ("3", True), ("3", False)]:
                run(sys.argv[1], sys.argv[2], root, major, enabled)
        finally:
            compositor.terminate()
            compositor.wait(timeout=10)
