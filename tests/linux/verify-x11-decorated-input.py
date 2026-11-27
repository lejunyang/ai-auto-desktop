#!/usr/bin/env python3
"""Check GTK3 input effects under xfwm4 in a private Xvfb and D-Bus session.

Run with /usr/bin/python3 and pass the aad and x11_xtest_helper binaries.
Requires GTK3 GI bindings, xauth, Xvfb, xfwm4, xprop, setxkbmap,
and a C compiler with X11/XTest development headers for the XRecord observer.
"""
from contextlib import ExitStack
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


def control_client(control):
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

    return call


def drag_fixture(icon, role="both", obstructed=False):
    import gi

    gi.require_version("Gtk", "3.0")
    gi.require_version("Gdk", "3.0")
    from gi.repository import Gdk, GLib, Gtk

    def make_window(title, name, size):
        window = Gtk.Window(title=title)
        window.set_default_size(*size)
        window.set_resizable(False)
        surface = Gtk.EventBox()
        surface.get_accessible().set_name(name)
        layout = Gtk.Fixed()
        window.add(surface)
        surface.add(layout)
        return window, layout

    cross = role != "both"
    separate = cross or icon == "two-windows"
    window, layout = make_window("AAD GTK drag source", "Trial drag surface",
                                 (270, 180) if separate else (680, 400))
    if separate:
        target_window, target_layout = make_window("AAD GTK drag destination", "Trial destination surface",
                                                   (270, 180))
        window.move(275, 420)
        target_window.move(275, 140)
    else:
        target_window, target_layout = window, layout
    source = Gtk.ListBox()
    source.set_selection_mode(Gtk.SelectionMode.NONE)
    source.set_size_request(136, 58)
    layout.put(source, 45, 60 if separate else 169)
    item = Gtk.EventBox()
    item.set_size_request(128, 52)
    item.add(Gtk.Label(label="Trial list item"))
    source.add(item)
    row = source.get_row_at_index(0)
    row.get_accessible().set_name("Trial list row")
    slot = Gtk.EventBox()
    slot.get_accessible().set_name("Destination slot")
    slot.set_size_request(136, 58)
    target_layout.put(slot, 45 if separate else 493, 60 if separate else 169)
    destination = Gtk.ListBox()
    destination.set_selection_mode(Gtk.SelectionMode.NONE)
    slot.add(destination)
    placeholder = Gtk.Label(label="Empty slot")
    destination.add(placeholder)
    status = Gtk.Label(label="Placement source count 0")
    layout.put(status, 20 if separate else 232, 135 if separate else 335)
    targets = [Gtk.TargetEntry.new("application/x-aad-trial-list-item",
                                   0 if cross else Gtk.TargetFlags.SAME_APP, 1)]
    item.drag_source_set(Gdk.ModifierType.BUTTON1_MASK, targets, Gdk.DragAction.MOVE)
    slot.drag_dest_set(Gtk.DestDefaults.MOTION | Gtk.DestDefaults.HIGHLIGHT, targets, Gdk.DragAction.MOVE)
    payload = b"aad-trial-item"
    placements = 0
    drops = 0
    cover = None

    def begin(_item, context):
        nonlocal cover
        if icon != "default" and not cross:
            hotspot = -35 if separate else 0
            Gtk.drag_set_icon_widget(context, Gtk.Label(label="Moving trial row"), hotspot, hotspot)
        if obstructed:
            origin = item.get_window().get_origin()
            cover = Gtk.Window(type=Gtk.WindowType.POPUP)
            cover.set_default_size(100, 60)
            if cross:
                cover.move(origin.x + 20, origin.y - 150)
            else:
                cover.move(origin.x + 190, origin.y)
            cover.show()

    def place():
        nonlocal placements
        source.remove(row)
        destination.remove(placeholder.get_parent())
        destination.add(row)
        destination.show_all()
        placements += 1
        status.set_text(f"Placement destination count {placements}")
        return False

    def drop(widget, context, _x, _y, time_):
        target = widget.drag_dest_find_target(context, None)
        if target == Gdk.Atom.intern("NONE", False):
            return False
        widget.drag_get_data(context, target, time_)
        return True

    def receive(_slot, context, _x, _y, data, _info, time_):
        nonlocal drops
        drops += 1
        accepted = data.get_data() == payload and row.get_parent() == source and drops == 1
        Gtk.drag_finish(context, accepted, accepted and cross, time_)
        if accepted:
            GLib.idle_add(place)

    def read(stream, _condition):
        if not stream.readline():
            Gtk.main_quit()
            return False
        print(json.dumps({"drops": drops, "placements": placements, "source": len(source.get_children()),
                          "destination": row.get_parent() == destination}), flush=True)
        return True

    item.connect("drag-begin", begin)
    item.connect("drag-data-get", lambda _, _ctx, data, _info, _time: data.set(data.get_target(), 8, payload))
    if role == "source":
        item.connect("drag-data-delete", lambda *_: source.remove(row))
    slot.connect("drag-drop", drop)
    slot.connect("drag-data-received", receive)
    if role != "destination":
        window.show_all()
    if separate and role != "source":
        target_window.show_all()
    GLib.io_add_watch(sys.stdin, GLib.PRIORITY_DEFAULT, GLib.IO_IN | GLib.IO_HUP, read)
    Gtk.main()


def straight_drags(aad, recorder):
    def center(bounds):
        return tuple(bounds[p] + bounds[s] // 2 for p, s in (("x", "width"), ("y", "height")))

    def contains(bounds, point):
        x, y = point
        return (bounds["x"] <= x < bounds["x"] + bounds["width"]
                and bounds["y"] <= y < bounds["y"] + bounds["height"])

    with subprocess.Popen([aad, "control", "serve", "--allow-input"], stdin=subprocess.PIPE,
                          stdout=subprocess.PIPE, text=True) as control:
        call = control_client(control)
        try:
            assert "data" in call("capabilities")
            for icon, cross, obstructed in (("default", False, False), ("custom", False, False),
                                            ("two-windows", False, False), ("default", False, True),
                                            ("default", True, False), ("default", True, True)):
                case = "cross-process default" if cross else icon
                if obstructed:
                    case += " with cover"
                with ExitStack() as fixtures:
                    def start_fixture(role):
                        gtk = fixtures.enter_context(subprocess.Popen(
                            [sys.executable, __file__, "--drag-fixture", icon, role, str(int(obstructed))],
                            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True))
                        fixtures.callback(gtk.terminate)
                        return gtk

                    source = start_fixture("source" if cross else "both")
                    target = start_fixture("destination") if cross else source

                    def resource_for(gtk):
                        apps = lambda: [r for r in call("targets")["data"]["resources"]
                                        if r["metadata"].get("process_id") == gtk.pid]
                        return try_until(apps, lambda rows: len(rows) == 1)[0]["resource_id"]

                    def observe(resource):
                        response = call("observe", resource_id=resource, view={"mode": "nodes"})
                        assert not response.get("error") and not response["data"]["truncated"], response
                        return {node["name"]: node for node in response["data"]["nodes"]}

                    source_resource = resource_for(source)
                    target_resource = resource_for(target) if cross else source_resource
                    nodes = observe(source_resource)
                    target_nodes = observe(target_resource) if cross else nodes
                    assert "Placement source count 0" in nodes
                    surface = nodes["Trial drag surface"]
                    destination = target_nodes.get("Trial destination surface", surface)
                    start = center(nodes["Trial list row"]["bounds"])
                    end = center(target_nodes["Destination slot"]["bounds"])

                    def fraction(node, point):
                        b = node["bounds"]
                        return {"fx": (point[0] - b["x"]) / b["width"],
                                "fy": (point[1] - b["y"]) / b["height"]}

                    with subprocess.Popen([recorder], stdout=subprocess.PIPE, text=True) as record:
                        try:
                            assert record.stdout.readline() == "ready\n", "XRecord setup failed"
                            result = call("act", target=surface["target"],
                                          action={"type": "drag", "to": destination["target"],
                                                  "fraction": fraction(surface, start),
                                                  "to_fraction": fraction(destination, end)})
                            if result["effect"] == "not_applied":
                                record.terminate()
                            output, _ = record.communicate(timeout=5)
                        finally:
                            if record.poll() is None:
                                record.kill()
                    trace = [tuple(map(int, line.split())) for line in output.splitlines()]
                    buttons = [e for e in trace if e[0] in (4, 5)]
                    moves = [e for e in trace if e[0] == 6 and e[4] & 256]
                    def receipt(gtk):
                        gtk.stdin.write("read\n")
                        gtk.stdin.flush()
                        return json.loads(gtk.stdout.readline())

                    print(f"GTK {case}: effect={result['effect']}, moves={len(moves)}, buttons={buttons}", flush=True)
                    if result["effect"] != "applied":
                        error = result["error"]
                        print(f"GTK {case}: {error['code']} {error['details'].get('helper_code', '')}; "
                              f"receipt={receipt(target)}", flush=True)
                    assert [(e[0], e[3]) for e in buttons] == [(4, 1), (5, 1)], trace
                    if obstructed:
                        assert result["effect"] == "unknown" and result.get("error"), result
                        assert 0 < len(moves) < 12 and buttons[1][1:3] != end, trace
                        assert receipt(target)["drops"] == 0 and receipt(source)["source"] == 1
                        print(f"PASS decorated GTK {case}: stopped before destination and released", flush=True)
                        continue
                    assert result["effect"] == "applied", result
                    assert len(moves) == 12 and moves[-1][1:3] == buttons[1][1:3], trace
                    assert buttons[0][1:3] == start and buttons[1][1:3] == end, trace
                    if icon == "two-windows":
                        frame = nodes["AAD GTK drag source"]["bounds"]
                        client = surface["bounds"]
                        title_hits = [e[1:3] for e in moves[:-1] if contains(frame, e[1:3]) and e[2] < client["y"]]
                        assert title_hits, (frame, client, moves)
                        print(f"GTK two-windows: held moves through source title bar {title_hits}", flush=True)
                    if not cross:
                        final = try_until(lambda: observe(target_resource),
                                          lambda rows: "Placement destination count 1" in rows)
                    else:
                        try_until(lambda: receipt(source), lambda data: data["source"] == 0)
                        try_until(lambda: receipt(target), lambda data: data["placements"] == 1)
                        final = observe(target_resource)
                    assert contains(final["Destination slot"]["bounds"], center(final["Trial list row"]["bounds"]))
                    assert receipt(target) == {
                        "drops": 1, "placements": 1, "source": 0, "destination": True
                    }
                    print(f"PASS decorated GTK {case}: 12 held moves, one drop and one placement", flush=True)
        finally:
            control.terminate()


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
        call = control_client(control)

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

            def act(node, action, xid):
                response = call("act", target=node["target"], action={"type": action})
                assert response["effect"] == "applied", response
                result = response["data"]["backend_result"]["backend_result"]
                assert result["window"] == int(xid, 16) and result["window_binding"] == "geometry", result

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

            def rejected(response):
                assert response.returncode == 73, response
                reply = json.loads(response.stdout)
                assert reply["dispatch_started"] is False, reply

            rejected(helper("window-info"))
            rejected(helper("window-info", "--geometry-proof", "1"))
            act(observe(SIBLING), "minimize", sibling)
            assert state(sibling) == (True, False)
            active = xprop("-root", "_NET_ACTIVE_WINDOW")
            for name in ("close", "minimize", "maximize", "restore", "activate"):
                rejected(helper("window-action", "--action", name))
                assert state(sibling) == (True, False) and xprop("-root", "_NET_ACTIVE_WINDOW") == active
            print("PASS single-client mismatched window actions dispatched nothing and left EWMH state unchanged", flush=True)
            act(observe(SIBLING), "restore", sibling)
            assert state(sibling) == (False, False)
            print("PASS minimized client with matching geometry restored from a fresh observation", flush=True)
        finally:
            control.terminate()
            gtk.terminate()


def run(aad, recorder):
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
            straight_drags(aad, recorder)
        finally:
            wm.terminate()
            try:
                wm.wait(timeout=3)
            except subprocess.TimeoutExpired:
                wm.kill()


if __name__ == "__main__":
    if sys.argv[1] == "--fixture":
        fixture(sys.argv[2] == "decorated")
    elif sys.argv[1] == "--drag-fixture":
        drag_fixture(sys.argv[2], sys.argv[3], sys.argv[4] == "1")
    elif sys.argv[1] == "--window-fixture":
        window_fixture()
    elif sys.argv[1] == "--private":
        run(sys.argv[2], sys.argv[3])
    else:
        aad, helper = (str(Path(path).resolve(strict=True)) for path in sys.argv[1:])
        with tempfile.TemporaryDirectory(prefix="aad-x11-decorated-") as profile:
            recorder = str(Path(profile) / "x11-pointer-events")
            flags = subprocess.check_output(["pkg-config", "--cflags", "--libs", "x11", "xtst"], text=True).split()
            subprocess.run(["cc", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror",
                            str(Path(__file__).with_name("x11_pointer_events.c")),
                            "-o", recorder, *flags], check=True)
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
                            "dbus-run-session", "--", sys.executable, __file__, "--private", aad, recorder],
                           env=env, check=True, timeout=120)
