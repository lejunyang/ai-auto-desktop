"""Verify X11 element focus through aad MCP, on a private Xvfb and D-Bus."""
import argparse
from contextlib import ExitStack
import ctypes as c
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

PROVIDER = "desktop.linux_atspi"


def emit(stream, kind, **data):
    print(json.dumps(dict(monotonic_ns=time.monotonic_ns(), kind=kind, **data)),
          file=stream, flush=True)


class X11:
    def __init__(self):
        self.api = c.CDLL("libX11.so.6")
        unsigned, integer, pointer = c.c_ulong, c.c_int, c.c_void_p
        for name, result, args in (
            ("XOpenDisplay", pointer, [c.c_char_p]),
            ("XSetInputFocus", integer, [pointer, unsigned, integer, unsigned]),
            ("XSync", integer, [pointer, integer]),
            ("XGetInputFocus", integer, [pointer, c.POINTER(unsigned), c.POINTER(integer)]),
            ("XQueryKeymap", integer, [pointer, c.c_char_p]),
            ("XInternAtom", unsigned, [pointer, c.c_char_p, integer]),
            ("XGetSelectionOwner", unsigned, [pointer, unsigned]),
        ):
            fn = getattr(self.api, name)
            fn.restype, fn.argtypes = result, args
        self.display = self.api.XOpenDisplay(None)
        assert self.display

    def focused(self):
        window, revert = c.c_ulong(), c.c_int()
        self.api.XGetInputFocus(self.display, c.byref(window), c.byref(revert))
        return window.value

    def focus(self, window):
        self.api.XSetInputFocus(self.display, window, 1, 0)
        self.api.XSync(self.display, 0)
        assert self.focused() == window

    def keys_down(self):
        keys = c.create_string_buffer(32)
        assert self.api.XQueryKeymap(self.display, keys)
        return [i for i in range(256) if keys.raw[i // 8] & (1 << i % 8)]

    def clipboard_owner(self):
        atom = self.api.XInternAtom(self.display, b"CLIPBOARD", 0)
        return self.api.XGetSelectionOwner(self.display, atom)


def fixture(role, state_file):
    import gi
    gi.require_version("Gtk", "3.0")
    gi.require_version("Gdk", "3.0")
    gi.require_version("GdkX11", "3.0")
    from gi.repository import Gdk, GdkX11, GLib, Gtk
    x = X11()
    owner = role == "owner"
    window = Gtk.Window(title="Interrupted input target" if owner else "Interrupted input sink")
    window.set_default_size(400, 230)
    window.move(32 if owner else 545, 45)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=10, margin=12)
    window.add(box)
    entries = {}
    switched = set()
    delayed = set()
    current_case = None
    trigger = state_file.with_suffix(".switch")

    def event(kind, **data):
        current = json.loads(state_file.read_text())
        emit(sys.stdout, kind, case=current["case"], role=role, **data)
        return current

    def keyboard(_widget, ev, tag, phase):
        current = event("key_" + phase, target=tag, key=Gdk.keyval_name(ev.keyval),
                        keycode=ev.hardware_keycode, state=int(ev.state), x_time_ms=ev.time)
        if owner and tag == "Target entry" and phase == "down" and current.get("queue_delay_ms"):
            if current["case"] not in delayed:
                delayed.add(current["case"])
                # Keep X events queued while the main loop can still answer AT-SPI.
                source = GLib.main_current_source()
                assert source.get_name().startswith("GDK X11 Event source")
                priority = source.get_priority()
                source.set_priority(GLib.PRIORITY_LOW)
                resume_at = time.monotonic() + current["queue_delay_ms"] / 1000

                def resume_x_events():
                    if time.monotonic() < resume_at:
                        return True
                    source.set_priority(priority)
                    event("x_queue_resumed")
                    return False

                GLib.idle_add(resume_x_events)
        if owner and tag == "Target entry" and phase == "up" and current.get("switch"):
            if current["case"] not in switched:
                switched.add(current["case"])
                if current["switch"] == "sibling":
                    sibling = entries["Sibling entry"]
                    sibling.grab_focus()
                    event("focus_trigger", from_target=tag, to_target="Sibling entry",
                          sibling_has_focus=sibling.has_focus(), xid=x.focused())
                else:
                    x.focus(current["sink_xid"])
                    event("focus_trigger", from_target=tag, to_target="Sink entry", xid=x.focused())
        return False

    def focus_event(_widget, _ev, tag, kind):
        event(kind, target=tag)
        return False

    for tag in (["Target entry", "Sibling entry"] if owner else ["Sink entry"]):
        entry = Gtk.Entry()
        entries[tag] = entry
        entry.get_accessible().set_name(tag)
        entry.connect("key-press-event", keyboard, tag, "down")
        entry.connect("key-release-event", keyboard, tag, "up")
        entry.connect("changed", lambda widget, name: event("text", target=name, value=widget.get_text()), tag)
        entry.connect("focus-in-event", focus_event, tag, "focus_in")
        entry.connect("focus-out-event", focus_event, tag, "focus_out")
        box.pack_start(entry, False, False, 0)
    def monitor_case():
        nonlocal current_case
        case = json.loads(state_file.read_text())["case"]
        if case != current_case:
            current_case = case
            for entry in entries.values():
                entry.set_text("")
            event("reset")
        if owner and trigger.exists():
            entries["Sibling entry"].grab_focus()
            event("focus_trigger", to_target="Sibling entry",
                  sibling_has_focus=entries["Sibling entry"].has_focus(), xid=x.focused())
            trigger.unlink()
        return True

    GLib.timeout_add(3, monitor_case)
    window.show_all()
    next(iter(entries.values())).grab_focus()
    window.get_display().sync()
    emit(sys.stdout, "ready", role=role, xid=GdkX11.X11Window.get_xid(window.get_window()), pid=os.getpid())
    Gtk.main()


def stop(process):
    if process.stdin:
        process.stdin.close()
    try:
        process.wait(timeout=1)
    except subprocess.TimeoutExpired:
        process.terminate()
        try:
            process.wait(timeout=2)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait()


def until(action, what, seconds=10):
    end = time.monotonic() + seconds
    while time.monotonic() < end:
        value = action()
        if value:
            return value
        time.sleep(0.005)
    raise RuntimeError("timed out waiting for " + what)


def read_rows(path):
    return [json.loads(line) for line in path.read_text().splitlines() if line.endswith("}")]


class MCP:
    def __init__(self, process, log):
        self.process = process
        self.log = log
        self.sequence = 0
        self.responses = {}
        threading.Thread(target=self.read, daemon=True).start()

    def read(self):
        for line in self.process.stdout:
            row = json.loads(line)
            self.responses[row["id"]] = row

    def send(self, method, params, notification=False):
        self.sequence += 1
        call = dict(jsonrpc="2.0", method=method, params=params)
        if not notification:
            call["id"] = self.sequence
        emit(self.log, "send", rpc=call)
        self.process.stdin.write(json.dumps(call) + "\n")
        self.process.stdin.flush()
        return self.sequence

    def recv(self, sequence):
        row = until(lambda: self.responses.get(sequence), f"MCP reply {sequence}", 15)
        emit(self.log, "reply", rpc=row)
        assert "error" not in row, row
        result = row["result"]
        return json.loads(result["content"][0]["text"]) if "content" in result else result

    def tool(self, name, wait=True, **arguments):
        sequence = self.send("tools/call", dict(name="control_" + name, arguments=arguments))
        return self.recv(sequence) if wait else sequence


def helper_proxy(state_file, helper, args):
    """Switch focus at a native preflight or first element proof."""
    current = json.loads(state_file.read_text())
    case = current["case"]
    if case == "first_stroke_cross_window" and args[0] == "focus-info":
        x = X11()
        assert x.focused() == current["owner_xid"]
        x.focus(current["sink_xid"])
    if case in {"paste_refused", "paste_timed_out", "paste_owner_timeout", "paste_owner_cancel"} and args[0] == "paste":
        deadline = args[args.index("--deadline-monotonic-ns") + 1]
        os.environ.update(
            LD_PRELOAD=str(state_file.with_name("paste-restore-delay.so")),
            AAD_TEST_RESTORE_STARTED=str(state_file.with_suffix(f".{case}.restore")),
        )
        if case == "paste_timed_out":
            os.environ["AAD_TEST_RESTORE_AFTER_NS"] = deadline
        if case in {"paste_owner_timeout", "paste_owner_cancel"}:
            os.environ["AAD_TEST_OWNER_REPLY"] = str(state_file.with_suffix(f".{case}.owner-reply"))
        if case == "paste_owner_timeout":
            os.environ["AAD_TEST_OWNER_REPLY_AFTER_NS"] = deadline
    if case not in {"first_stroke", "chord_refused", "paste_refused"} or "--anchor-token" not in args:
        os.execv(helper, [helper, *args])
    with subprocess.Popen([helper, *args], stdout=subprocess.PIPE, text=True) as child:
        for line in child.stdout:
            frame = json.loads(line)
            if frame.get("event") == "validate_segment" and frame["index"] == 0:
                if case == "paste_refused":
                    state_file.with_suffix(".clipboard-owner").write_text(str(X11().clipboard_owner()))
                trigger = state_file.with_suffix(".switch")
                trigger.touch()
                until(lambda: not trigger.exists(), "same-window focus switch", seconds=2)
            print(line, end="", flush=True)
        return child.wait()


def trial(aad, helper, out, baseline, scenario):
    with tempfile.TemporaryDirectory(prefix="aad-element-focus-") as scratch, ExitStack() as stack:
        root = Path(scratch)
        for name in ("RUNTIME", "CACHE_HOME", "DATA_HOME", "CONFIG_HOME", "STATE_HOME"):
            child = root / name.lower()
            child.mkdir(mode=0o700)
            os.environ["XDG_RUNTIME_DIR" if name == "RUNTIME" else f"XDG_{name}"] = str(child)
        os.environ.update(GDK_BACKEND="x11", XDG_SESSION_TYPE="x11", GTK_IM_MODULE="simple")
        for key in ("AT_SPI_BUS_ADDRESS", "WAYLAND_DISPLAY", "NO_AT_BRIDGE", "GTK_A11Y",
                    "GTK_MODULES", "QT_IM_MODULE", "XMODIFIERS"):
            os.environ.pop(key, None)
        events = {role: out / f"{role}.ndjson" for role in ("owner", "sink")}
        state_file = root / "case.json"
        if not baseline:
            subprocess.run(["cc", "-std=c11", "-Wall", "-Wextra", "-Werror", "-shared", "-fPIC",
                            str(Path(__file__).with_name("x11_paste_restore_delay.c")),
                            "-ldl", "-o", str(root / "paste-restore-delay.so")], check=True)
        proxy = root / "helper"
        proxy_args = [sys.executable, str(Path(__file__).resolve()), "helper-proxy",
                      str(state_file), helper]
        proxy.write_text(f"#!{sys.executable}\nimport os, sys\n"
                         f"os.execv({sys.executable!r}, {proxy_args!r} + sys.argv[1:])\n")
        proxy.chmod(0o700)
        os.environ["AAD_LINUX_XTEST_HELPER"] = str(proxy)
        log = stack.enter_context((out / "mcp.ndjson").open("w"))
        checkpoints = stack.enter_context((out / "checks.ndjson").open("w"))
        diagnostics = stack.enter_context((out / "diagnostics.log").open("w"))

        def set_case(name, **values):
            updated = state_file.with_suffix(".tmp")
            updated.write_text(json.dumps(dict(case=name, **values)))
            updated.replace(state_file)
            emit(checkpoints, "case", case=name)

        def rows(role, case):
            return [r for r in read_rows(events[role]) if r.get("case") == case]

        set_case("startup")
        fixtures = {}
        for role, path in events.items():
            stream = stack.enter_context(path.open("w"))
            app = subprocess.Popen([sys.executable, __file__, "fixture", role, str(state_file)],
                                   stdout=stream, stderr=diagnostics)
            stack.callback(stop, app)
            fixtures[role] = until(lambda: next((r for r in read_rows(path) if r["kind"] == "ready"), None),
                                   f"{role} GTK fixture")
        x = X11()
        x.focus(fixtures["owner"]["xid"])

        server = subprocess.Popen([aad, "mcp", "--provider", PROVIDER, "--allow-input"],
                                  stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                  stderr=diagnostics, text=True)
        stack.callback(stop, server)
        mcp = MCP(server, log)
        mcp.recv(mcp.send("initialize", dict(protocolVersion="2025-03-26", capabilities={},
                                            clientInfo=dict(name="aad-x11-interruption-trial", version="1"))))
        mcp.send("notifications/initialized", {}, notification=True)
        mcp.tool("capabilities")

        def find_resource():
            reply = mcp.tool("targets", provider=PROVIDER)
            found = [r for r in (reply.get("data") or {}).get("resources", [])
                     if r["metadata"].get("process_id") == fixtures["owner"]["pid"]]
            assert len(found) <= 1, found
            return found[0]["resource_id"] if found else None

        resource = until(find_resource, "owner's AT-SPI resource")
        emit(checkpoints, "fixture", display=os.environ["DISPLAY"], windows=fixtures, resource=resource)

        def observed_nodes():
            reply = mcp.tool("observe", resource_id=resource, max_nodes=100, view=dict(mode="nodes"))
            assert not reply.get("error"), reply
            return reply["data"]["nodes"]

        def entries():
            return {node["name"]: node for node in observed_nodes()
                    if node.get("name") in {"Target entry", "Sibling entry"}}

        def run(case, action, target=None, timeout_ms=10000, before_reply=None, **switch):
            x.focus(fixtures["owner"]["xid"])
            set_case(case, **switch)
            until(lambda: all(any(r["kind"] == "reset" for r in rows(role, case))
                              for role in events), f"{case} reset")
            if target is None:
                target = entries()["Target entry"]["target"]
            start = time.monotonic_ns()
            pending = mcp.tool("act", wait=False, request_id=case, target=target,
                               action=action, timeout_ms=timeout_ms)
            if before_reply is not None:
                before_reply(case, pending)
            reply = mcp.recv(pending)
            elapsed_ms = (time.monotonic_ns() - start) / 1e6
            values = {name: node.get("value") for name, node in entries().items()}
            widgets = rows("owner", case) + rows("sink", case)
            presses = [row for row in widgets if row["kind"] == "key_down"]
            span_ms = (presses[-1]["monotonic_ns"] - presses[0]["monotonic_ns"]) / 1e6 if presses else 0
            held = x.keys_down()
            emit(checkpoints, "outcome", case=case, response=reply, values=values,
                 elapsed_ms=elapsed_ms, key_span_ms=span_ms, presses=len(presses), keys_down=held)
            print(f"{case}: {reply.get('effect')}, {len(presses)} presses, "
                  f"{elapsed_ms:.1f} ms MCP, {span_ms:.1f} ms between first and last press", flush=True)
            assert not held, held
            return reply, values, presses

        def type_text(case, text, mode="unicode", **kwargs):
            return run(case, dict(type="type_text", text=text, mode=mode), **kwargs)

        def typed(case, text, mode="unicode"):
            reply, values, presses = type_text(case, text, mode)
            assert reply.get("effect") == "applied", reply
            assert values == {"Target entry": text, "Sibling entry": ""}, values
            assert all(row["target"] == "Target entry" for row in presses), presses
            if not baseline:
                progress = reply["data"]["backend_result"]["backend_result"]["input"]["input_progress"]
                assert progress["delivered"] == progress["total"] == len(text) and progress["released"], progress

        def denied(reply, code, cause, delivered, total, effect):
            error = reply["error"]
            assert error["code"] == code and reply["effect"] == effect, reply
            assert error["details"].get(cause[0]) == cause[1], reply
            progress = error["details"]["input_progress"]
            assert (progress["delivered"], progress["total"], progress["released"]) == (delivered, total, True), reply

        def first_stroke_cross_window():
            reply, values, presses = type_text("first_stroke_cross_window", "abc",
                                               owner_xid=fixtures["owner"]["xid"],
                                               sink_xid=fixtures["sink"]["xid"])
            error = reply["error"]
            assert reply["effect"] == "not_applied" and error["code"] == "DRIVER.FOCUS_LOST", reply
            details = error["details"]
            assert (details["helper_code"], details["dispatch_started"], details["proven_window"]) == (
                "focus_owner_changed", False, fixtures["owner"]["xid"]), reply
            assert values == {"Target entry": "", "Sibling entry": ""} and not presses, (values, presses)

        def copy_original(*options):
            previous_owner = x.clipboard_owner()
            subprocess.run(["xclip", "-selection", "clipboard", *options], input=b"original clip", check=True)

            def new_owner():
                owner = x.clipboard_owner()
                return owner if owner and owner != previous_owner else None

            # xclip can return after forking but before the child owns CLIPBOARD.
            return until(new_owner, "xclip to take clipboard ownership")

        def read_clipboard(case, *options):
            read = subprocess.run(["xclip", "-selection", "clipboard", "-o", *options], capture_output=True)
            emit(checkpoints, "clipboard_read", case=case, status=read.returncode,
                 text=read.stdout.decode(), owner=x.clipboard_owner())
            return read.returncode, read.stdout

        def paste_unreadable():
            # SAVE_TARGETS is clipboard-manager metadata, so the helper skips it.
            original_owner = copy_original("-target", "SAVE_TARGETS")
            assert read_clipboard("paste_unreadable_targets", "-target", "TARGETS") == (
                0, b"TARGETS\nSAVE_TARGETS\n")
            reply, values, _ = run("paste_unreadable", dict(type="paste", text="must not be inserted"))
            assert read_clipboard("paste_unreadable", "-target", "SAVE_TARGETS") == (0, b"original clip")
            assert x.clipboard_owner() == original_owner
            assert values == {"Target entry": "", "Sibling entry": ""}, values
            assert not any(row["kind"] in {"key_down", "key_up"}
                           for role in events for row in rows(role, "paste_unreadable"))
            denied(reply, "DRIVER.CLIPBOARD_BUSY", ("helper_code", "clipboard_unreadable"),
                   0, 1, "not_applied")
            error = reply["error"]
            assert error["details"]["clipboard_restored"] == "untouched", reply
            assert error["details"]["dispatch_started"] is False, reply
            assert "could not be saved" in error["message"] and "nothing was pasted" in error["message"], reply
            assert "fill or type_text" in error["hint"], reply

        def paste_recovery():
            def cancel_at_confirmation(case, request):
                until(state_file.with_suffix(f".{case}.owner-reply").exists, "clipboard takeover")
                mcp.send("notifications/cancelled", {"requestId": request}, notification=True)

            def interrupt_ownership(case, timeout_ms, before_reply=None):
                copy_original()
                reply, values, _ = run(case, dict(type="paste", text="must not be inserted"),
                                       timeout_ms=timeout_ms, before_reply=before_reply)
                clipboard = read_clipboard(case)
                held_confirmation = state_file.with_suffix(f".{case}.owner-reply").exists()
                restorer_started = state_file.with_suffix(f".{case}.restore").exists()
                emit(checkpoints, "clipboard_confirm", case=case, held_confirmation=held_confirmation,
                     restorer_started=restorer_started)
                keys = [row for role in events for row in rows(role, case)
                        if row["kind"] in {"key_down", "key_up"}]
                return reply, values, keys, clipboard, held_confirmation, restorer_started

            empty = {"Target entry": "", "Sibling entry": ""}
            original = (0, b"original clip")

            original_owner = copy_original()
            refused, refused_values, presses = run("paste_refused", dict(type="paste", text="must not be inserted"))
            refused_clipboard = read_clipboard("paste_refused")
            owner_at_proof = int(state_file.with_suffix(".clipboard-owner").read_text())
            emit(checkpoints, "clipboard_at_proof", case="paste_refused", original_owner=original_owner,
                 owner_at_proof=owner_at_proof,
                 restorer_started=state_file.with_suffix(".paste_refused.restore").exists())
            assert refused_values == empty and not presses, (refused_values, presses)

            copy_original()
            delayed, values, presses = run("paste_timed_out", dict(type="paste", text="粘贴"), timeout_ms=1200)
            delayed_clipboard = read_clipboard("paste_timed_out")
            delayed_restorer = state_file.with_suffix(".paste_timed_out.restore").exists()
            timed_out_at_owner = interrupt_ownership("paste_owner_timeout", 1200)
            cancelled_at_owner = interrupt_ownership("paste_owner_cancel", 10000, cancel_at_confirmation)

            assert delayed_restorer, delayed
            assert values == {"Target entry": "粘贴", "Sibling entry": ""} and presses, (values, presses)
            assert owner_at_proof == original_owner and refused_clipboard == original, refused
            assert not state_file.with_suffix(".paste_refused.restore").exists(), refused
            denied(refused, "DRIVER.FOCUS_LOST", ("cause", "focused_element_changed"), 0, 1, "not_applied")
            assert refused["error"]["details"]["clipboard_restored"] == "untouched", refused
            assert delayed_clipboard == original, delayed
            denied(delayed, "DRIVER.TIMEOUT", ("clipboard_restored", "restored"), 1, 1, "unknown")

            for result, code in ((timed_out_at_owner, "DRIVER.TIMEOUT"),
                                 (cancelled_at_owner, "DRIVER.CANCELLED")):
                reply, values, keys, clipboard, held_confirmation, restorer_started = result
                assert held_confirmation and restorer_started, result
                assert values == empty and not keys and clipboard == original, result
                denied(reply, code, ("clipboard_restored", "restored"), 0, 1, "not_applied")
                assert reply["error"]["details"]["dispatch_started"] is False, reply

            reply, values, presses = run("paste_reobserved", dict(type="paste", text="粘贴"))
            assert reply["effect"] == "applied" and values == {"Target entry": "粘贴", "Sibling entry": ""}, reply
            assert read_clipboard("paste_reobserved") == original

        if scenario == "window-preflight":
            first_stroke_cross_window()
            typed("window_preflight_reobserved", "abc")
            return
        if scenario == "paste-recovery":
            paste_recovery()
            return
        if scenario == "paste-unreadable":
            paste_unreadable()
            return
        if scenario == "stroke-barrier":
            for mode in ("unicode", "keys"):
                for index in range(9):
                    case = f"loaded_{mode}_{index + 1}"
                    reply, values, presses = type_text(case, "abcdefghi", mode, switch="sibling",
                                                      queue_delay_ms=35)
                    focus = until(lambda: next((r for r in rows("owner", case)
                                                if r["kind"] == "focus_trigger"), None), case)
                    assert focus["sibling_has_focus"] and focus["xid"] == fixtures["owner"]["xid"], focus
                    assert any(r["kind"] == "x_queue_resumed" for r in rows("owner", case)), case
                    if not baseline:
                        details = reply["error"]["details"]
                        reason = ("cause", "focused_element_changed")
                        if details.get("helper_code") == "text_stroke_unconfirmed":
                            reason = ("helper_code", "text_stroke_unconfirmed")
                        denied(reply, "DRIVER.UNKNOWN_EFFECT", reason, 1, 9, "unknown")
                        assert values == {"Target entry": "a", "Sibling entry": ""}, values
                        assert [(r["target"], r["key"]) for r in presses] == [("Target entry", "a")], presses
            if not baseline:
                reply, values, presses = type_text("unconfirmed_stroke", "a汉b", queue_delay_ms=450)
                denied(reply, "DRIVER.UNKNOWN_EFFECT", ("helper_code", "text_stroke_unconfirmed"), 1, 3, "unknown")
                assert values == {"Target entry": "a", "Sibling entry": ""}, values
                assert [(r["target"], r["key"]) for r in presses] == [("Target entry", "a")], presses
                until(lambda: any(r["kind"] == "x_queue_resumed" for r in rows("owner", "unconfirmed_stroke")),
                      "GTK to resume its X event queue")
            return
        if scenario == "typing-speed":
            for index in range(3):
                typed(f"timing_{index + 1}", ("abcdefghi" * 114)[:1024])
            return

        old = entries()["Target entry"]["target"]
        assert old != entries()["Target entry"]["target"]
        reply, _, presses = type_text("stale", "stale-input", target=old)
        assert reply["error"]["code"] == "CONTROL.STALE_TARGET" and not presses, reply
        for mode, text in [("unicode", "abcdefghi"), ("keys", "aBcdEfgHi")]:
            case = f"sibling_{mode}"
            reply, values, presses = type_text(case, text, mode, switch="sibling")
            focus = next(r for r in rows("owner", case) if r["kind"] == "focus_trigger")
            assert focus["xid"] == fixtures["owner"]["xid"] and focus["sibling_has_focus"], focus
            if baseline:
                assert reply["effect"] == "applied" and reply["commit_state"] == "committed", reply
                assert values == {"Target entry": text[0], "Sibling entry": text[1:]}, values
            else:
                denied(reply, "DRIVER.UNKNOWN_EFFECT", ("cause", "focused_element_changed"), 1, 9, "unknown")
                assert values == {"Target entry": text[0], "Sibling entry": ""}, values
                assert [(r["target"], r["key"]) for r in presses] == [("Target entry", text[0])], presses
            typed(f"reobserved_{mode}", text, mode)

        typed("unicode_non_ascii", "a汉éΩ")
        typed("long_text", ("abcdefghi" * 114)[:1024])
        if not baseline:
            first_stroke_cross_window()
            reply, values, presses = type_text("cross_process", "klmnopqrs", switch="window",
                                               sink_xid=fixtures["sink"]["xid"])
            denied(reply, "DRIVER.UNKNOWN_EFFECT", ("helper_code", "focus_owner_changed"), 1, 9, "unknown")
            assert values == {"Target entry": "k", "Sibling entry": ""}, values
            assert [(r["target"], r["key"]) for r in presses] == [("Target entry", "k")], presses
            typed("cross_process_reobserved", "normal")

            for case, action, total in (
                ("first_stroke", dict(type="type_text", text="abc"), 3),
                ("chord_refused", dict(type="key", keys=["ctrl", "a"]), 1),
            ):
                reply, values, presses = run(case, action)
                denied(reply, "DRIVER.FOCUS_LOST", ("cause", "focused_element_changed"), 0, total, "not_applied")
                assert values == {"Target entry": "", "Sibling entry": ""} and not presses, (values, presses)
            paste_unreadable()
            paste_recovery()
            reply, _, presses = run("chord_reobserved", dict(type="key", keys=["f2"]))
            assert reply["effect"] == "applied" and [r["key"] for r in presses] == ["F2"], reply


if __name__ == "__main__":
    mode = sys.argv[1] if len(sys.argv) > 1 else None
    if mode == "fixture":
        fixture(sys.argv[2], Path(sys.argv[3]))
    elif mode == "helper-proxy":
        sys.exit(helper_proxy(Path(sys.argv[2]), sys.argv[3], sys.argv[4:]))
    elif mode == "private-session":
        if (os.environ["DISPLAY"] in {":10", os.environ["AAD_CALLER_DISPLAY"]}
                or os.environ["DBUS_SESSION_BUS_ADDRESS"] == os.environ["AAD_CALLER_BUS"]
                or os.environ["XAUTHORITY"] == os.environ["AAD_CALLER_XAUTHORITY"]):
            raise RuntimeError("the fixture requires a private Xvfb and D-Bus session")
        trial(sys.argv[2], sys.argv[3], Path(sys.argv[4]), "--baseline" in sys.argv[6:], sys.argv[5])
    else:
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument("aad", type=Path)
        parser.add_argument("helper", type=Path)
        parser.add_argument("output", type=Path)
        parser.add_argument("--baseline", action="store_true")
        parser.add_argument("--scenario", choices=["full", "window-preflight", "paste-recovery", "paste-unreadable",
                                                  "stroke-barrier", "typing-speed"], default="full")
        args = parser.parse_args()
        if args.baseline and args.scenario not in {"full", "stroke-barrier", "typing-speed"}:
            parser.error("--baseline requires the full, stroke-barrier or typing-speed scenario")
        args.output.mkdir(parents=True, exist_ok=True)
        env = dict(os.environ, AAD_CALLER_DISPLAY=os.environ.get("DISPLAY", ""),
                   AAD_CALLER_BUS=os.environ.get("DBUS_SESSION_BUS_ADDRESS", ""),
                   AAD_CALLER_XAUTHORITY=os.environ.get("XAUTHORITY", ""))
        command = ["xvfb-run", "-a", "--server-args=-screen 0 1024x768x24 -nolisten tcp",
                   "dbus-run-session", "--", sys.executable, str(Path(__file__).resolve()),
                   "private-session", str(args.aad.resolve()), str(args.helper.resolve()),
                   str(args.output.resolve()), args.scenario]
        if args.baseline:
            command.append("--baseline")
        sys.exit(subprocess.call(command, env=env))
