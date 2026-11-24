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

        mouse = []
        mask = Gdk.EventMask.BUTTON_PRESS_MASK | Gdk.EventMask.BUTTON_RELEASE_MASK

        def watch_mouse(widget, name, on_press=None):
            widget.get_accessible().set_name(name)
            def phase(_widget, event, kind):
                mouse.append([name, kind, int(event.button),
                              bool(event.state & Gdk.ModifierType.BUTTON1_MASK)])
                if kind == "press" and on_press:
                    on_press()
                return False
            widget.connect("button-press-event", phase, "press")
            widget.connect("button-release-event", phase, "release")

        button = Gtk.Button(label="Post invoice")
        watch_mouse(button, "Post invoice")
        button.connect("clicked", lambda *_: clicks.append("posted"))
        box.add(button)

        def drawing(name, on_press=None):
            area = Gtk.DrawingArea()
            area.set_size_request(250, 32)
            area.add_events(mask)
            watch_mouse(area, name, on_press)
            return area

        canvas = Gtk.Overlay()
        canvas.add(drawing("Invoice canvas"))
        edge = Gtk.EventBox()
        edge.set_size_request(35, -1)
        edge.set_halign(Gtk.Align.START)
        watch_mouse(edge, "Edge cover")
        canvas.add_overlay(edge)
        box.add(canvas)
        changing = Gtk.Overlay()
        curtain = Gtk.EventBox()
        curtain.set_no_show_all(True)
        watch_mouse(curtain, "New cover")
        changing_button = Gtk.Button(label="Cover on press")
        armed = True
        cancelled = []
        watch_mouse(changing_button, "Cover on press", lambda: curtain.show() if armed else None)
        changing_button.connect("grab-broken-event", lambda w, e: cancelled.append(
            bool(w.get_state_flags() & Gtk.StateFlags.ACTIVE)) or False)
        changing_button.connect("clicked", lambda *_: clicks.append("restored"))
        changing.add(changing_button)
        changing.add_overlay(curtain)
        box.add(changing)
        def uncover(*_):
            nonlocal armed
            armed = False
            curtain.hide()
        remove_cover = Gtk.Button(label="Remove cover")
        remove_cover.connect("clicked", uncover)
        box.add(remove_cover)
        incomplete = Gtk.DrawingArea()
        incomplete.set_size_request(250, 15)
        incomplete.add_events(Gdk.EventMask.BUTTON_PRESS_MASK)
        watch_mouse(incomplete, "Press only")
        box.add(incomplete)

        def show_closing(*_):
            popup = Gtk.Window(title="Close on press test", transient_for=window)
            popup.add(drawing("Close on press", popup.close))
            popup.connect("destroy", lambda *_: clicks.append("closed"))
            popup.show_all()
            popup.present()

        opener = Gtk.Button(label="Open closing window")
        opener.connect("clicked", show_closing)
        box.add(opener)
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
                        wheel=wheel, quantity=scale.get_value(), phases=phases, mouse=mouse, clicks=clicks,
                        cancelled=cancelled, pressed=bool(changing_button.get_state_flags() & Gtk.StateFlags.ACTIVE))
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
        return json.loads(result.stdout)

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
            names = ("Post invoice", "Invoice canvas", "Cover on press")
            nodes = {n.get("name"): n["capabilities"] for n in call("snapshot", resource)["nodes"]}
            for name in names:
                assert ("click" in nodes[name]) == enabled, (name, nodes)
            assert "click" not in nodes["Press only"], nodes
            call("do", "click", resource, "--name", "Press only", success=False)
            if enabled:
                expected = []
                for name, args in ((names[0], []), (names[1], ["--fx", ".76", "--fy", ".4"])):
                    call("do", "click", resource, "--name", name, *args)
                    expected.extend([[name, "press", 1, False], [name, "release", 1, True]])
                    wait_for(lambda: state()["mouse"] == expected, name + " button phases")
                assert state()["clicks"] == ["posted"]
                for args in (["--fx", ".02", "--fy", ".5"], ["--button", "right"], ["--count", "2"]):
                    call("do", "click", resource, "--name", names[1], *args, success=False)
                assert state()["mouse"] == expected
                for name in (names[2], "Close on press"):
                    if name == "Close on press":
                        call("do", "invoke", resource, "--name", "Open closing window")
                        wait_for(lambda: any(n.get("name") == name and "click" in n["capabilities"]
                            for n in call("snapshot", resource)["nodes"]), "closing window ready")
                    failure = call("do", "click", resource, "--name", name, success=False)["error"]
                    assert failure["effect"] == "unknown", failure
                    expected.append([name, "press", 1, False])
                    wait_for(lambda: state()["mouse"] == expected, name + " withheld release")
                    if name == "Cover on press":
                        covered = next(n for n in call("snapshot", resource)["nodes"] if n.get("name") == name)
                        assert "click" not in covered["capabilities"], covered
                        call("do", "click", resource, "--name", name, success=False)
                        current = state()
                        assert current["cancelled"] == [True] and not current["pressed"], current
                        assert current["clicks"] == ["posted"] and current["mouse"] == expected, current
                        call("do", "invoke", resource, "--name", "Remove cover")
                        call("do", "click", resource, "--name", name)
                        expected.extend([[name, "press", 1, False], [name, "release", 1, True]])
                        wait_for(lambda: state()["mouse"] == expected and state()["clicks"] == ["posted", "restored"]
                                 and not state()["pressed"], "uncovered button recovers on next click")
                wait_for(lambda: state()["clicks"] == ["posted", "restored", "closed"], "closed on press")
                time.sleep(.12)
                assert state()["mouse"] == expected
            else:
                for name in names:
                    call("do", "click", resource, "--name", name, success=False)
                assert not state()["mouse"] and not state()["clicks"]
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
    except Exception:
        if receipt.exists():
            print(receipt.read_text(), file=sys.stderr, flush=True)
        raise
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
