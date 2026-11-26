#!/usr/bin/env python3
"""Check GTK3 input effects under xfwm4 in a private Xvfb and D-Bus session.

Run with /usr/bin/python3 and pass the aad and x11_xtest_helper binaries.
Requires GTK3 GI bindings, xauth, Xvfb, xfwm4, xprop and setxkbmap.
"""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import time


ENTRY = "aad geometry entry"


def fixture(decorated):
    import gi

    gi.require_version("Gtk", "3.0")
    from gi.repository import GLib, Gtk

    window = Gtk.Window(title="aad GTK geometry fixture")
    window.set_decorated(decorated)
    window.set_default_size(480, 120)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, margin=20)
    window.add(box)
    entry = Gtk.Entry()
    entry.get_accessible().set_name(ENTRY)
    box.pack_start(entry, False, False, 0)
    button = Gtk.Button(label="Other focus")
    box.pack_start(button, False, False, 0)
    events = []
    entry.connect("button-press-event", lambda _, e: events.append(["click", e.x_root, e.y_root]) or False)
    entry.connect("key-press-event", lambda _, e: events.append(["down", e.hardware_keycode]) or False)
    entry.connect("key-release-event", lambda _, e: events.append(["up", e.hardware_keycode]) or False)
    entry.connect("changed", lambda _: events.append(["text", entry.get_text()]))

    def reply(record):
        print(json.dumps(record, ensure_ascii=False), flush=True)

    def handle(stream, _condition):
        request = stream.readline().strip()
        if not request:
            Gtk.main_quit()
            return False
        if request == "reset":
            entry.set_text("")
            button.grab_focus()
            events.clear()
        origin = window.get_window().get_origin()
        x, y = entry.translate_coordinates(window, 0, 0)
        size = entry.get_allocation()
        reply({
            "bounds": {"x": origin.x + x, "y": origin.y + y,
                       "width": size.width, "height": size.height},
            "focus": entry.has_focus(), "text": entry.get_text(),
            "caret": entry.get_position(), "events": events,
        })
        return True

    window.show_all()
    button.grab_focus()
    GLib.io_add_watch(sys.stdin, GLib.PRIORITY_DEFAULT, GLib.IO_IN | GLib.IO_HUP, handle)
    Gtk.main()


def try_until(query, accept):
    deadline = time.monotonic() + 12
    while time.monotonic() < deadline:
        result = query()
        if accept(result):
            return result
        time.sleep(0.1)
    raise AssertionError(f"private fixture did not become ready: {result}")


def run(aad):
    def cli(*args):
        return json.loads(subprocess.check_output([aad, "--compact", *args], text=True, timeout=15))

    with subprocess.Popen(["xfwm4", "--compositor=off"], stdout=subprocess.DEVNULL) as wm:
        try:
            try_until(lambda: subprocess.check_output(
                ["xprop", "-root", "_NET_SUPPORTING_WM_CHECK"], text=True), lambda s: "window id" in s)
            subprocess.run(["setxkbmap", "-layout", "us"], check=True)
            for mode in ("decorated", "undecorated"):
                with subprocess.Popen([sys.executable, __file__, "--fixture", mode],
                                      stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                      text=True, encoding="utf-8") as gtk:
                    try:
                        apps = lambda: [a for a in cli("apps")["apps"]
                                        if a["metadata"].get("process_id") == gtk.pid]
                        resource = try_until(apps, lambda a: len(a) == 1)[0]["resource_id"]
                        target = (resource, "--name", ENTRY)

                        def read(request):
                            gtk.stdin.write(request + "\n")
                            gtk.stdin.flush()
                            return json.loads(gtk.stdout.readline())

                        before = read("reset")
                        assert cli("find", *target)["node"]["bounds"] == before["bounds"], before
                        cli("do", "click", *target, "--fx", "0.5", "--fy", "0.5")
                        after = read("read")
                        bounds = before["bounds"]
                        assert after["focus"] and after["caret"] == 0, after
                        assert after["events"] == [["click", bounds["x"] + bounds["width"] // 2,
                                                     bounds["y"] + bounds["height"] // 2]], after
                        print(f"PASS {mode} observed click {after['events'][0][1:]} in {bounds}", flush=True)
                        for text, options in (("AaLLoo!!, .? Zz", ("--mode", "keys")), ("你好，世界！你好。", ())):
                            read("reset")
                            cli("do", "type-text", *target, "--text", text, *options)
                            received = read("read")
                            assert received["text"] == cli("find", *target)["node"]["value"] == text, received
                            assert [v for kind, v in received["events"] if kind == "text"] == [
                                text[:index + 1] for index in range(len(text))
                            ], received
                            assert sorted(v for kind, v in received["events"] if kind == "down") == sorted(
                                v for kind, v in received["events"] if kind == "up"
                            ), received
                            print(f"PASS {mode} {'keys' if options else 'unicode'} {len(text)} characters", flush=True)
                    finally:
                        gtk.terminate()
        finally:
            wm.terminate()


if __name__ == "__main__":
    if sys.argv[1] == "--fixture":
        fixture(sys.argv[2] == "decorated")
    elif sys.argv[1] == "--private":
        run(sys.argv[2])
    else:
        aad, helper = (str(Path(path).resolve(strict=True)) for path in sys.argv[1:])
        with tempfile.TemporaryDirectory(prefix="aad-x11-decorated-") as profile:
            env = {**os.environ, "AAD_LINUX_XTEST_HELPER": helper, "GDK_BACKEND": "x11",
                   "XDG_SESSION_TYPE": "x11", "GTK_A11Y": "always", "GTK_IM_MODULE": "gtk-im-context-simple"}
            for key in ("DISPLAY", "XAUTHORITY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET",
                        "AT_SPI_BUS_ADDRESS", "DBUS_SESSION_BUS_ADDRESS", "SESSION_MANAGER",
                        "NO_AT_BRIDGE", "GDK_SCALE", "GDK_DPI_SCALE", "GTK_MODULES"):
                env.pop(key, None)
            for key in ("XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME"):
                isolated = Path(profile) / key
                isolated.mkdir(mode=0o700)
                env[key] = str(isolated)
            subprocess.run(["xvfb-run", "-a", "-n", "170", "-s", "-screen 0 1024x768x24 -nolisten tcp",
                            "dbus-run-session", "--", sys.executable, __file__, "--private", aad],
                           env=env, check=True, timeout=120)
