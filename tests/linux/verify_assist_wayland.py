#!/usr/bin/python3
"""Run from a checkout with target/debug/aad, GStreamer, PipeWire and WirePlumber 0.4.

Starts isolated D-Bus and media servers. The only producers draw red and blue
test pixels. A private ScreenCast service returns its selected node and a
connected FD. The real CLI's ambient PipeWire socket deliberately does not exist.
"""
import contextlib
import io
import json
import os
from pathlib import Path
import select
import signal
import socket
import subprocess
import sys
import tempfile
import time

import dbus
import dbus.service
from dbus.mainloop.glib import DBusGMainLoop
from gi.repository import GLib
from PIL import Image

ROOT = Path(__file__).resolve().parents[2]
sys.path.insert(0, str(ROOT / "tests/windows"))
from verify_assist import Viewer

BASE = "/org/freedesktop/portal/desktop"
SCREEN = "org.freedesktop.portal.ScreenCast"
SESSION = "org.freedesktop.portal.Session"
REQUEST = "org.freedesktop.portal.Request"
NAME = "org.freedesktop.portal.Desktop"


def portal(runtime):
    assert os.environ["DBUS_SESSION_BUS_ADDRESS"] == f"unix:path={runtime}/bus"
    DBusGMainLoop(set_as_default=True)
    bus = dbus.SessionBus(private=True)
    name = dbus.service.BusName(NAME, bus=bus, do_not_queue=True)
    objects = {}
    sessions = []

    def log(method):
        with (runtime / "calls").open("a") as out:
            print(method, file=out)

    class Request(dbus.service.Object):
        @dbus.service.signal(REQUEST, signature="ua{sv}")
        def Response(self, code, values):
            pass

        @dbus.service.method(REQUEST)
        def Close(self):
            log("Request.Close")

    class Session(dbus.service.Object):
        @dbus.service.signal(SESSION, signature="a{sv}")
        def Closed(self, details):
            pass

        @dbus.service.method(SESSION)
        def Close(self):
            log("Session.Close")

    class Portal(dbus.service.Object):
        @dbus.service.method("org.freedesktop.DBus.Properties", in_signature="ss", out_signature="v")
        def Get(self, interface, key):
            log(f"Get {interface}.{key}")
            assert interface == SCREEN
            return dbus.UInt32({"version": 4, "AvailableSourceTypes": 1}[key])

        def reply(self, sender, options, values, code=0):
            path = f'{BASE}/request/{sender[1:].replace(".", "_")}/{options["handle_token"]}'
            obj = Request(name, path)
            objects[path] = obj
            obj.Response(code, dbus.Dictionary(values, signature="sv"))
            return dbus.ObjectPath(path)

        @dbus.service.method(SCREEN, in_signature="a{sv}", out_signature="o", sender_keyword="sender")
        def CreateSession(self, options, sender):
            log("ScreenCast.CreateSession")
            path = f'{BASE}/session/{sender[1:].replace(".", "_")}/{options["session_handle_token"]}'
            objects[path] = Session(name, path)
            sessions.append(path)
            return self.reply(sender, options, {"session_handle": dbus.String(path)})

        @dbus.service.method(SCREEN, in_signature="oa{sv}", out_signature="o", sender_keyword="sender")
        def SelectSources(self, session, options, sender):
            log("ScreenCast.SelectSources")
            assert session == sessions[-1] and options["types"] == 1 and not options["multiple"]
            assert options.get("persist_mode", 0) == 0 and "restore_token" not in options
            return self.reply(sender, options, {})

        @dbus.service.method(SCREEN, in_signature="osa{sv}", out_signature="o", sender_keyword="sender")
        def Start(self, session, parent, options, sender):
            log("ScreenCast.Start")
            assert session == sessions[-1] and parent == ""
            data = json.loads((runtime / "selection").read_text())
            props = {"source_type": dbus.UInt32(data["type"]), "size": dbus.Struct((500, 400), signature="ii")}
            if data.get("serial"):
                props["pipewire-serial"] = dbus.UInt64(data["serial"])
            streams = dbus.Array([(dbus.UInt32(data["node"]), dbus.Dictionary(props, signature="sv"))],
                                 signature="(ua{sv})")
            return self.reply(sender, options, {"streams": streams}, data.get("deny", 0))

        @dbus.service.method(SCREEN, in_signature="oa{sv}", out_signature="h")
        def OpenPipeWireRemote(self, session, options):
            assert session == sessions[-1]
            log("ScreenCast.OpenPipeWireRemote")
            with socket.socket(socket.AF_UNIX) as endpoint:
                endpoint.connect(str(runtime / "pipewire-0"))
                return dbus.types.UnixFd(endpoint.fileno())

    instance = Portal(name, BASE)
    def revoke():
        objects[sessions[-1]].Closed(dbus.Dictionary({}, signature="sv"))
        return True
    GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, signal.SIGUSR1, revoke)
    (runtime / "serving").touch()
    GLib.MainLoop().run()


def wait_for(label, test):
    end = time.monotonic() + 10
    while time.monotonic() < end:
        result = test()
        if result:
            return result
        time.sleep(0.04)
    raise AssertionError(f"timeout waiting for {label}")


def main(runtime):
    env = dict(os.environ, XDG_RUNTIME_DIR=str(runtime), PIPEWIRE_RUNTIME_DIR=str(runtime),
               DBUS_SESSION_BUS_ADDRESS=f"unix:path={runtime}/bus", XDG_SESSION_TYPE="wayland",
               XDG_STATE_HOME=str(runtime / "state"), XDG_CONFIG_HOME=str(runtime / "config"),
               XDG_CACHE_HOME=str(runtime / "cache"))
    for key in ["DISPLAY", "WAYLAND_DISPLAY", "PIPEWIRE_REMOTE", "PIPEWIRE_CORE"]:
        env.pop(key, None)
    log = (runtime / "process.log").open("w+")
    with log, contextlib.ExitStack() as stack:
        def start(args, stdout=log, custom=env):
            child = subprocess.Popen(args, stdout=stdout, stderr=log, env=custom)
            def stop():
                if child.poll() is None:
                    child.terminate()
                try:
                    child.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    child.kill()
                    child.wait()
            stack.callback(stop)
            return child

        def nodes():
            entries = json.loads(subprocess.check_output(["pw-dump"], env=env, timeout=3))
            return {e["info"]["props"].get("node.name"): (e["id"], e["info"]["props"]["object.serial"])
                    for e in entries if e["type"] == "PipeWire:Interface:Node"}

        start(["dbus-daemon", "--session", "--nofork", "--nopidfile", f"--address=unix:path={runtime}/bus"])
        wait_for("private D-Bus", (runtime / "bus").exists)
        start(["pipewire"])
        wait_for("private PipeWire", (runtime / "pipewire-0").exists)
        # Policy only. This fixture does not load the hardware monitor scripts.
        start(["wireplumber", "-c", "policy.conf"])
        for pattern in ["blue", "red"]:
            start(["gst-launch-1.0", "-q", "videotestsrc", "is-live=true", f"pattern={pattern}", "!",
                   "video/x-raw,format=RGB,width=64,height=40,framerate=5/1", "!",
                   "pipewiresink", "mode=provide", f"stream-properties=props,node.name=aad-{pattern},media.class=Video/Source"])
        def producers():
            available = nodes()
            return available if {"aad-red", "aad-blue"} <= available.keys() else None
        available = wait_for("two private video nodes", producers)
        service = start([sys.executable, __file__, "--serve", str(runtime)])
        wait_for("private ScreenCast", (runtime / "serving").exists)
        binary = os.environ.get("AAD", str(ROOT / "target/debug/aad"))
        consumer = dict(env, PIPEWIRE_REMOTE="aad-no-ambient-remote")
        def assist():
            return start([binary, "--compact", "assist", "--desktop", "--view-only", "--codecs", "jpeg"],
                         subprocess.PIPE, consumer)
        select_file = runtime / "selection"
        for name, serial, revoke in [("aad-red", True, True), ("aad-blue", False, False)]:
            node, number = available[name]
            decoy = available["aad-blue" if name == "aad-red" else "aad-red"][1]
            consumer.update(PIPEWIRE_NODE=str(decoy), PIPEWIRE_PROPS=f"target.object={decoy}")
            select_file.write_text(json.dumps({"node": node, "type": 1, "serial": number if serial else None}))
            child = assist()
            assert select.select([child.stdout], [], [], 22)[0], "assist startup timeout"
            announce = json.loads(child.stdout.readline())
            assert announce.get("view_only") is True and announce["source"]["capture"] == "wayland-portal-monitor", announce
            viewer = Viewer(announce["websocket"])
            try:
                viewer.send({"type": "take"})
                viewer.send({"type": "key", "key": "F1", "code": "F1", "action": "down"})
                frame = viewer.frame()
                image = Image.open(io.BytesIO(frame["data"])).convert("RGB")
                pixel = image.getpixel((16, 16))
                assert image.size == (64, 40) and pixel[0 if serial else 2] > 240 and pixel[2 if serial else 0] < 12, pixel
            finally:
                viewer.close()
            if revoke:
                service.send_signal(signal.SIGUSR1)
            else:
                child.send_signal(signal.SIGINT)
            assert (child.wait(timeout=4) != 0) == revoke
        for case in [{"deny": 1, "type": 1}, {"type": 2}, {"type": 1, "node": 4294967295}]:
            before = (runtime / "calls").read_text().count("OpenPipeWireRemote")
            select_file.write_text(json.dumps({"node": available["aad-red"][0], **case}))
            child = assist()
            assert child.wait(timeout=5) != 0 and b'"websocket"' not in child.stdout.read()
            assert (runtime / "calls").read_text().count("OpenPipeWireRemote") == before
        # A vanished node or serial must never link to the remaining producer.
        for selection in [{"node": 999999}, {"node": available["aad-red"][0], "serial": 999999}]:
            select_file.write_text(json.dumps({"type": 1, **selection}))
            child = assist()
            assert child.wait(timeout=20) != 0 and b'"websocket"' not in child.stdout.read()
        calls = (runtime / "calls").read_text()
        assert "RemoteDesktop" not in calls and calls.count("Session.Close") == 7, calls
        print("verified selected monitor pixels, scoped FD, revocation, graceful close and five refusals")


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--serve":
        portal(Path(sys.argv[2]))
    else:
        with tempfile.TemporaryDirectory(prefix="aad-sc-") as d:
            try:
                main(Path(d))
            except BaseException:
                logfile = Path(d) / "process.log"
                if logfile.exists():
                    print(logfile.read_text(errors="replace")[-5000:], file=sys.stderr)
                raise
