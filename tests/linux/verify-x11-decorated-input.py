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
PRIMARY = "aad decorated primary"
SIBLING = "aad decorated sibling"


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


def window_fixture():
    import gi

    gi.require_version("Gtk", "3.0")
    gi.require_version("GdkX11", "3.0")
    from gi.repository import GdkX11, GLib, Gtk

    primary = Gtk.Window(title=PRIMARY)
    sibling = Gtk.Window(title=SIBLING)
    for window, x, width in ((primary, 40, 380), (sibling, 540, 320)):
        window.set_default_size(width, 140)
        window.move(x, 80)
        window.show_all()
    sibling.present()

    def handle(stream, _condition):
        request = stream.readline().strip()
        if request == "move":
            primary.move(100, 210)
        elif request == "present":
            primary.present()
        else:
            Gtk.main_quit()
            return False
        return True

    GLib.io_add_watch(sys.stdin, GLib.PRIORITY_DEFAULT, GLib.IO_IN | GLib.IO_HUP, handle)
    print(json.dumps([GdkX11.X11Window.get_xid(w.get_window()) for w in (primary, sibling)]), flush=True)
    Gtk.main()


def window_actions(aad):
    def xprop(*args):
        return subprocess.check_output(["xprop", *args], text=True, timeout=5)

    def state(xid):
        props = xprop("-id", xid, "WM_STATE", "_NET_WM_STATE")
        return ("Iconic" in props or "_NET_WM_STATE_HIDDEN" in props,
                "_NET_WM_STATE_MAXIMIZED_HORZ" in props and "_NET_WM_STATE_MAXIMIZED_VERT" in props)

    with (
        subprocess.Popen([sys.executable, __file__, "--window-fixture"], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, text=True) as gtk,
        subprocess.Popen([aad, "control", "serve", "--allow-input"], stdin=subprocess.PIPE,
                         stdout=subprocess.PIPE, text=True) as control,
    ):
        session = None
        sequence = 0

        def call(method, **fields):
            nonlocal session, sequence
            sequence += 1
            request = {"request_id": str(sequence), "timeout_ms": 6000,
                       "operation": {"method": method, **fields}}
            if session is not None:
                request["session_id"] = session
            control.stdin.write(json.dumps(request) + "\n")
            control.stdin.flush()
            response = json.loads(control.stdout.readline())
            assert response["request_id"] == str(sequence), response
            session = response["session_id"]
            return response

        try:
            assert "data" in call("capabilities")
            primary, sibling = (hex(id_) for id_ in json.loads(gtk.stdout.readline()))
            try_until(lambda: xprop("-root", "_NET_ACTIVE_WINDOW"), lambda text: sibling in text)
            apps = lambda: [r for r in call("targets")["data"]["resources"]
                            if r["metadata"].get("process_id") == gtk.pid]
            resource = try_until(apps, lambda rows: len(rows) == 1)[0]["resource_id"]

            def observe(name):
                response = call("observe", resource_id=resource, view={"mode": "nodes"})
                nodes = [n for n in response["data"]["nodes"] if n["name"] == name]
                assert len(nodes) == 1, response
                return nodes[0]

            def act(node, action, xid, binding="geometry"):
                response = call("act", target=node["target"], action={"type": action})
                assert response["effect"] == "applied", response
                result = response["data"]["backend_result"]["backend_result"]
                assert result["window"] == int(xid, 16) and result["window_binding"] == binding, result

            act(observe(PRIMARY), "activate", primary)
            assert primary in xprop("-root", "_NET_ACTIVE_WINDOW")
            assert state(sibling) == (False, False)
            act(observe(PRIMARY), "maximize", primary)
            assert state(primary) == (False, True) and state(sibling) == (False, False)
            act(observe(PRIMARY), "restore", primary)
            assert state(primary) == state(sibling) == (False, False)
            moved = observe(PRIMARY)
            get_geometry = lambda: subprocess.check_output(["xwininfo", "-id", primary], text=True, timeout=5)
            geometry = get_geometry()
            gtk.stdin.write("move\n")
            gtk.stdin.flush()
            try_until(get_geometry, lambda text: text != geometry)
            stale = call("act", target=moved["target"], action={"type": "close_window"})
            assert stale["effect"] == "not_applied" and "STALE" in stale["error"]["code"], stale
            assert primary in xprop("-root", "_NET_CLIENT_LIST")
            act(observe(PRIMARY), "minimize", primary)
            assert state(primary) == (True, False) and state(sibling) == (False, False)
            # Closed from a fresh, visible observation after moving the focus
            # back to this exact window in the private fixture.
            gtk.stdin.write("present\n")
            gtk.stdin.flush()
            try_until(lambda: state(primary), lambda current: current == (False, False))
            act(observe(PRIMARY), "close_window", primary)
            clients = xprop("-root", "_NET_CLIENT_LIST")
            assert primary not in clients and sibling in clients and state(sibling) == (False, False)
            print("PASS decorated same-PID activate, maximize, restore, minimize, close and stale rejection", flush=True)

            def helper(command, *options):
                return subprocess.run([os.environ["AAD_LINUX_XTEST_HELPER"], command,
                                       "--expected-pid", str(gtk.pid), "--x", "900", "--y", "720",
                                       "--width", "20", "--height", "20", "--deadline-monotonic-ns",
                                       str(time.monotonic_ns() + 3_000_000_000), *options],
                                      capture_output=True, text=True, timeout=5)

            observed = json.loads(helper("window-info", "--geometry-proof", "1").stdout)
            assert observed["window_binding"] == "single_client_fallback", observed
            assert not observed["supports"]["close"] and not observed["supports"]["minimize"], observed
            for name in ("close", "minimize", "maximize"):
                denied = helper("window-action", "--action", name)
                assert denied.returncode == 73 and json.loads(denied.stdout)["dispatch_started"] is False, denied
            act(observe(SIBLING), "minimize", sibling)
            assert state(sibling) == (True, False)
            restore = helper("window-action", "--action", "restore", "--window-id", str(int(sibling, 16)),
                             "--single-client-fallback", "1")
            reply = json.loads(restore.stdout.splitlines()[-1])
            assert restore.returncode == 0 and reply["reached"] and reply["window_binding"] == "single_client_fallback", reply
            assert state(sibling) == (False, False)
            act(observe(SIBLING), "minimize", sibling)
            restore_target = observe(SIBLING)
            act(restore_target, "restore", sibling,
                restore_target["provenance"]["window"]["window_binding"])
            assert state(sibling) == (False, False)
            act(observe(SIBLING), "minimize", sibling)
            activate = helper("window-action", "--action", "activate", "--window-id", str(int(sibling, 16)),
                              "--single-client-fallback", "1")
            reply = json.loads(activate.stdout.splitlines()[-1])
            assert activate.returncode == 0 and reply["reached"] and reply["window_binding"] == "single_client_fallback", reply
            assert state(sibling) == (False, False) and sibling in xprop("-root", "_NET_ACTIVE_WINDOW")
            print("PASS single-client fallback restore and activate; mismatched close, minimize and maximize rejected", flush=True)
        finally:
            control.terminate()
            gtk.terminate()


def run(aad):
    display = os.environ.get("DISPLAY", "").removeprefix(":")
    assert display.isdecimal() and int(display) >= 170
    assert os.environ.get("XAUTHORITY") and os.environ.get("DBUS_SESSION_BUS_ADDRESS")

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
            window_actions(aad)
        finally:
            wm.terminate()


if __name__ == "__main__":
    if sys.argv[1] == "--fixture":
        fixture(sys.argv[2] == "decorated")
    elif sys.argv[1] == "--window-fixture":
        window_fixture()
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
            for key in ("XDG_RUNTIME_DIR", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME",
                        "AAD_CONTROL_LEASE_DIR"):
                isolated = Path(profile) / key
                isolated.mkdir(mode=0o700)
                env[key] = str(isolated)
            subprocess.run(["xvfb-run", "-a", "-n", "170", "-s", "-screen 0 1024x768x24 -nolisten tcp",
                            "dbus-run-session", "--", sys.executable, __file__, "--private", aad],
                           env=env, check=True, timeout=120)
