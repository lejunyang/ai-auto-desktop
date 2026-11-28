"""Exercise the real CLI, control and MCP entry points in the private GTK fixture."""
import argparse
from contextlib import ExitStack, contextmanager
import hashlib
import json
import os
from pathlib import Path
import re
import select
import statistics
import subprocess
import time

PROVIDER = "desktop.linux_atspi"
FIXTURE_PROCESS = "gtk-timed-click-fixture"
START = (140, 220)
GROUPS = (("NW", 280, 160), ("NN", 280, 64), ("FW", 740, 160), ("FN", 740, 64))
SEED = 14
BUTTON1_MASK = 1 << 8


def until(read, description, timeout=8):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if result := read():
            return result
        time.sleep(0.01)
    raise AssertionError(f"timed out: {description}")


def run(*argv):
    return subprocess.check_output(argv, text=True, timeout=15).strip()


@contextmanager
def process(argv, **options):
    child = subprocess.Popen(argv, **options)
    try:
        yield child
    finally:
        if child.poll() is None:
            child.terminate()
            try:
                child.wait(timeout=3)
            except subprocess.TimeoutExpired:
                child.kill()
                child.wait()


class JsonConnection:
    def __init__(self, child, log):
        self.child = child
        self.log = log
        self.next_id = 0
        self.replies = {}

    def send(self, body, sample, rpc=False):
        self.next_id += 1
        key = "id" if rpc else "request_id"
        body[key] = str(self.next_id)
        self.log.write(json.dumps({"sample": sample, "sent_us": time.monotonic_ns() // 1000,
                                   "request": body}) + "\n")
        self.child.stdin.write((json.dumps(body) + "\n").encode())
        return body[key]

    def read(self, ticket, sample):
        while ticket not in self.replies:
            assert select.select([self.child.stdout], [], [], 12)[0], f"no response for {ticket}"
            reply = json.loads(self.child.stdout.readline())
            self.log.write(json.dumps({"sample": sample, "received_us": time.monotonic_ns() // 1000,
                                       "response": reply}) + "\n")
            self.replies[str(reply.get("id", reply.get("request_id")))] = reply
        return self.replies.pop(ticket)


class Control(JsonConnection):
    def __init__(self, child, log):
        super().__init__(child, log)
        self.session_id = None
        self.call("capabilities")

    def request(self, method, sample=None, timeout=3000, **arguments):
        body = {"timeout_ms": timeout, "operation": {"method": method, **arguments}}
        if self.session_id:
            body["session_id"] = self.session_id
        return self.send(body, sample)

    def call(self, method, sample=None, **arguments):
        reply = self.read(self.request(method, sample, **arguments), sample)
        assert reply.get("error") is None, reply
        self.session_id = reply["session_id"]
        return reply["data"]

    def target(self, resource, locator, sample=None):
        observed = self.call("observe", sample, resource_id=resource, max_nodes=128)
        return self.call("find", sample, observation_id=observed["observation_id"], locator=locator)["target"]


class Mcp(JsonConnection):
    def rpc(self, method, params, sample=None):
        packet = {"jsonrpc": "2.0", "method": method, "params": params}
        reply = self.read(self.send(packet, sample, rpc=True), sample)
        assert "error" not in reply, reply
        return reply["result"]

    def tool(self, name, arguments, sample=None):
        reply = self.rpc("tools/call", {"name": name, "arguments": arguments}, sample)
        body = json.loads(reply["content"][0]["text"])
        assert not reply["isError"], body
        return body


class Gtk:
    def __init__(self, child, log):
        self.child = child
        self.log = log
        self.events = []
        self.stream = log.open()
        self.ready = self.wait("ready")
        self.xid = str(self.ready["xid"])

    def sync(self):
        for line in self.stream:
            self.events.append(json.loads(line))

    def wait(self, kind, name=None, after=0):
        def lookup():
            self.sync()
            return next((event for event in self.events[after:] if event["kind"] == kind
                         and (kind != "ready" or event["pid"] == self.child.pid)
                         and (name is None or event.get("sample") == name)), None)
        return until(lookup, f"{kind}: {name}")

    def command(self, line):
        self.child.stdin.write((line + "\n").encode())
        self.child.stdin.flush()

    def begin(self, name):
        run("xdotool", "mousemove", *map(str, START))
        time.sleep(0.06)
        self.command(f"begin {name}")
        started = self.wait("begin", name)
        assert (started["x"], started["y"]) == START, started
        return started

    def end(self, name, started, activations, held=False):
        time.sleep(0.1)
        self.command(f"end {name}")
        finished = self.wait("end", name)
        assert finished["count"] == started["count"] + activations, finished
        assert bool(finished["state"] & BUTTON1_MASK) == held, finished
        result = [event for event in self.events if event.get("sample") == name]
        assert not any(event["kind"] == "decoy_clicked" for event in result), result
        assert not any(event["kind"] in ("double_press", "triple_press") for event in result), result
        time.sleep(0.23)
        return result


def timed(speed=1.0, **extra):
    return {"type": "click", "input_timing": {"seed": SEED, "speed": speed}, **extra}


def receipt(reply):
    return reply["backend_result"]["input_timing"]


def physical(events, center, expected_moves=None):
    moves = [event for event in events if event["kind"] == "motion"]
    down, = [event for event in events if event["kind"] == "down"]
    up, = [event for event in events if event["kind"] == "up"]
    assert len([x for x in events if x["kind"] == "clicked"]) == 1, events
    assert all((event["x"], event["y"]) == center for event in (moves[-1], down, up)), events
    assert down["button"] == up["button"] == 1 and up["state"] & BUTTON1_MASK, events
    if expected_moves is not None:
        assert len(moves) == expected_moves, moves
    return moves, down, up


def schema_check(value):
    if isinstance(value, dict):
        assert not ({"x", "y", "point", "coordinates", "position"} & value.get("properties", {}).keys()), value
        for child in value.values():
            schema_check(child)
    elif isinstance(value, list):
        for child in value:
            schema_check(child)


def matrix(control, gtk, resource, locator, center, output, sample_count):
    durations = {}
    for mode in ("default", 1.0, 2.0):
        previous_points = None
        spans = []
        for attempt in range(sample_count):
            name = f"{locator['name']}-{mode}-{attempt + 1}"
            target = control.target(resource, locator, name)
            beginning = gtk.begin(name)
            reply = control.call("act", name, target=target,
                                 action={"type": "click"} if mode == "default" else timed(mode))
            events = gtk.end(name, beginning, 1)
            moves, down, up = physical(events, center, 1 if mode == "default" else None)
            span = moves[-1]["x_ms"] - moves[0]["x_ms"]
            settle = down["x_ms"] - moves[-1]["x_ms"]
            hold = up["x_ms"] - down["x_ms"]
            plans = None
            if mode == "default":
                assert hold <= 15 and up["x_ms"] - moves[0]["x_ms"] <= 25, events
            else:
                plans = receipt(reply)
                assert plans["seed"] == SEED and plans["speed"] == mode and plans["planner_version"] == 1, plans
                assert len(moves) >= 8 and len(moves) == plans["moves"], (name, plans, moves)
                assert plans["movement_ms"] - 65 <= span <= plans["movement_ms"] + 120, (span, plans)
                assert plans["settle_ms"] - 2 <= settle <= plans["settle_ms"] + 90, (settle, plans)
                assert plans["hold_ms"] - 2 <= hold <= plans["hold_ms"] + 90, (hold, plans)
                points = [(event["x"], event["y"]) for event in moves]
                assert any(abs(y - center[1]) >= 2 for _, y in points), points
                hops = [abs(b[0] - a[0]) for a, b in zip(points, points[1:])]
                assert hops[-1] < max(hops), hops
                assert plans["overshoot"] and max(x for x, _ in points) > center[0], points
                assert all(x < gtk.ready["box"][0] + gtk.ready["box"][2]
                           for x, _ in points if x > center[0]), points
                assert previous_points is None or points == previous_points, points
                previous_points = points
            spans.append(span)
            output.write(json.dumps({"sample": name, "motion_x11_ms": span, "settle_x11_ms": settle,
                                     "hold_x11_ms": hold, "motion_gtk_us": moves[-1]["rx_us"] - moves[0]["rx_us"],
                                     "hold_gtk_us": up["rx_us"] - down["rx_us"], "plan": plans}) + "\n")
        durations[mode] = statistics.median(spans)
    assert durations[2.0] < durations[1.0], durations
    return durations


def public_and_interruptions(aad, open_control, mcp, gtk, resource, locator, center, log):
    def cli(sample, *argv):
        result = subprocess.run([aad, *argv], capture_output=True, text=True, timeout=15)
        log.write(json.dumps({"sample": sample, "argv": argv, "stdout": result.stdout,
                              "stderr": result.stderr, "exit_code": result.returncode}) + "\n")
        assert result.returncode == 0, result.stdout or result.stderr
        return json.loads(result.stdout)

    def sample(name, perform, activations=1):
        beginning = gtk.begin(name)
        response = perform()
        return response, gtk.end(name, beginning, activations)

    def act(action):
        return control.call("act", target=control.target(resource, locator), action=action)

    result, events = sample("cli", lambda: cli("cli", "do", "click", resource, "--name", locator["name"],
                                              "--human", "--seed", str(SEED), "--speed", "1.5", "--hold-ms", "95"))
    assert receipt(result["result"])["speed"] == 1.5
    _, down, up = physical(events, center)
    assert 95 - 2 <= up["x_ms"] - down["x_ms"] <= 95 + 90
    result, events = sample("generated-seed", lambda: cli("generated-seed", "do", "click", resource,
                            "--name", locator["name"], "--human"))
    assert 0 <= receipt(result["result"])["seed"] < 2**32
    physical(events, center)

    control = open_control()
    control.call("targets", provider=PROVIDER)
    _, events = sample("semantic-invoke", lambda: act({"type": "invoke"}))
    assert not ({"down", "up", "motion"} & {event["kind"] for event in events}), events
    target = control.target(resource, locator)
    control.call("observe", resource_id=resource)
    reply, events = sample("stale-reference", lambda: control.read(
        control.request("act", target=target, action=timed()), None), 0)
    assert reply["effect"] == "not_applied" and reply["error"], reply
    assert not ({"down", "up", "motion"} & {event["kind"] for event in events}), events

    box = gtk.ready["box"]
    # The shared fraction resolver insets edge requests by one pixel.
    target_edge = (box[0] + box[2] - 2, center[1])
    reply, events = sample("edge-suppresses-overshoot", lambda: act(timed(fraction={"fx": 1.0, "fy": 0.5})))
    assert receipt(reply)["overshoot"] is False
    moves, _, _ = physical(events, target_edge)
    assert max(move["x"] for move in moves) == target_edge[0]
    control.call("close")

    tools = mcp.rpc("tools/list", {})["tools"]
    for tool in tools:
        schema_check(tool["inputSchema"])
    catalogue = {tool["name"]: tool["inputSchema"] for tool in tools}
    for schema in (catalogue["control_act"]["properties"]["action"],
                   catalogue["run_steps"]["properties"]["steps"]["items"]["properties"]["action"],
                   catalogue["save_workflow"]["properties"]["steps"]["items"]["properties"]["action"]):
        click = next(v for v in schema["oneOf"] if v["properties"]["type"]["const"] == "click")
        props = click["properties"]["input_timing"]["properties"]
        assert props["seed"]["maximum"] == 2**32 - 1 and props["speed"]["minimum"] == 0.5
    mcp.tool("control_targets", {"provider": PROVIDER})
    observed = mcp.tool("control_observe", {"resource_id": resource})["data"]
    target = mcp.tool("control_find", {"observation_id": observed["observation_id"], "locator": locator})["data"]["target"]
    packet = {"jsonrpc": "2.0", "method": "tools/call", "params": {"name": "control_act", "arguments": {
        "target": target, "action": {"type": "click", "input_timing": {"point": {"x": 400, "y": 300}}}}}}
    name = "mcp-refuses-coordinate"
    assert mcp.read(mcp.send(packet, name, rpc=True), name)["error"]["code"] == -32602
    reply, events = sample("mcp-act", lambda: mcp.tool("control_act", {"target": target, "action": timed()}))
    assert receipt(reply["data"])["seed"] == SEED
    physical(events, center)

    step = {"id": "press", "app": {"process": FIXTURE_PROCESS, "provider": PROVIDER},
            "locator": locator, "action": timed()}
    inline, events = sample("mcp-inline", lambda: mcp.tool("run_steps", {"steps": [step]}))
    assert inline["steps"][0]["input_timing"]["seed"] == SEED
    physical(events, center)
    mcp.tool("save_workflow", {"name": "timed_click_fixture", "steps": [step]})
    described = mcp.tool("describe_workflow", {"name": "timed_click_fixture"})
    assert described["actions"][0]["parameters"]["input_timing"] == step["action"]["input_timing"]

    before = len(gtk.events)
    gtk.command("move")
    gtk.ready = gtk.wait("ready", after=before)
    box = gtk.ready["box"]
    moved = (box[0] + box[2] // 2, box[1] + box[3] // 2)
    assert moved != center
    replay, events = sample("mcp-replay-moved", lambda: mcp.tool("run_workflow", {"name": "timed_click_fixture"}))
    assert replay["status"] == "succeeded" and replay["steps"][0]["input_timing"]["seed"] == SEED
    physical(events, moved)
    mcp.tool("save_workflow", {"name": "timed_click_cancel",
                             "steps": [{**step, "action": timed(hold_ms=750)}]})
    started = gtk.begin("mcp-cancel-replay")
    ticket = mcp.send({"jsonrpc": "2.0", "method": "tools/call", "params": {
        "name": "run_workflow", "arguments": {"name": "timed_click_cancel"}}}, "mcp-cancel-replay", rpc=True)
    gtk.wait("down", "mcp-cancel-replay")
    notification = {"jsonrpc": "2.0", "method": "notifications/cancelled",
                    "params": {"requestId": ticket}}
    log.write(json.dumps({"sample": "mcp-cancel-replay", "request": notification}) + "\n")
    mcp.child.stdin.write((json.dumps(notification) + "\n").encode())
    cancelled = json.loads(mcp.read(ticket, "mcp-cancel-replay")["result"]["content"][0]["text"])
    assert cancelled["status"] != "succeeded" and cancelled["error"]["effect"] == "unknown", cancelled
    assert cancelled["error"]["details"]["input_progress"]["released"] is True, cancelled
    gtk.end("mcp-cancel-replay", started, 0)
    mcp.tool("control_close", {})
    replay, events = sample("cli-replay-moved", lambda: cli("cli-replay-moved", "run", "timed_click_fixture",
                            "--permission", "desktop.input", "--permission", "desktop.observe"))
    assert replay["step_input_timing"]["press"]["input_timing"]["seed"] == SEED
    physical(events, moved)

    control = open_control()
    control.call("targets", provider=PROVIDER)
    target = control.target(resource, locator)
    reply, events = sample("budget-short", lambda: control.read(
        control.request("act", timeout=220, target=target, action=timed()), None), 0)
    assert reply["effect"] == "not_applied" and reply["error"], reply
    assert not ({"down", "up", "motion"} & {event["kind"] for event in events}), events

    target = control.target(resource, locator)
    run("xdotool", "mousedown", "1")
    try:
        started = gtk.begin("already-held")
        reply = control.read(control.request("act", target=target, action=timed()), "already-held")
        assert reply["effect"] == "not_applied", reply
        held_events = gtk.end("already-held", started, 0, held=True)
        assert not ({"down", "up", "motion"} & {event["kind"] for event in held_events}), held_events
    finally:
        run("xdotool", "mouseup", "1")

    for name, phase, interrupt in (
        ("cancel-in-motion", "motion", None),
        ("cancel-held", "down", None),
        ("cover-target", "motion", "cover-target"),
        ("focus-other-control", "down", "focus-other-control"),
        ("focus-other-window", "down", "focus-other-window"),
    ):
        target = control.target(resource, locator, name)
        started = gtk.begin(name)
        ticket = control.request("act", name, target=target, action=timed(hold_ms=750))
        gtk.wait(phase, name)
        if interrupt is None:
            ack = control.call("cancel", name, request_id=ticket)
            assert ack["cancellation_requested"], ack
        else:
            gtk.command(interrupt)
        error = control.read(ticket, name)
        events = gtk.end(name, started, 0)
        assert error["effect"] == "unknown" and error["error"], error
        progress = error["error"]["details"]["input_progress"]
        assert progress["released"] is True and progress["down_attempted"] == (phase == "down"), error
        assert len([event for event in events if event["kind"] == "up"]) == (phase == "down"), events
        if interrupt == "cover-target":
            gtk.command("uncover-target")
        run("xdotool", "windowactivate", "--sync", gtk.xid)

    _, events = sample("recovery", lambda: act(timed()))
    physical(events, moved)
    control.call("close")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("aad", type=Path)
    parser.add_argument("fixture", type=Path)
    parser.add_argument("output", type=Path)
    parser.add_argument("--sample-count", type=int, default=5)
    args = parser.parse_args()
    display = os.environ.get("DISPLAY", "")
    assert re.fullmatch(r":[0-9]+", display) and int(display[1:]) >= 118, display
    assert Path(os.environ["XAUTHORITY"]).is_file() and os.environ["DBUS_SESSION_BUS_ADDRESS"]
    helper = Path(os.environ["AAD_LINUX_XTEST_HELPER"])
    files = (args.aad, args.fixture, helper)
    env = {"display": display, "commit": run("git", "rev-parse", "HEAD"),
           "dirty": run("git", "status", "--short"), "aad_version": run(args.aad, "--version"),
           "helper": json.loads(run(helper, "describe")), "gtk": run("pkg-config", "--modversion", "gtk+-3.0"),
           "sha256": {str(path): hashlib.sha256(path.read_bytes()).hexdigest() for path in files}}
    (args.output / "environment.json").write_text(json.dumps(env, indent=2) + "\n")
    gtk_logs = args.output / "gtk-events.jsonl"
    transcripts = args.output / "public-replies.jsonl"
    with ExitStack() as stack:
        log = stack.enter_context(transcripts.open("w", buffering=1))
        errors = stack.enter_context((args.output / "processes.log").open("w"))
        samples = stack.enter_context((args.output / "samples.jsonl").open("w", buffering=1))
        events = stack.enter_context(gtk_logs.open("w", buffering=1))
        wm = stack.enter_context(process(["xfwm4", "--compositor=off"], stdout=errors, stderr=errors))
        until(lambda: re.search("0x[1-9a-f]", run("xprop", "-root", "_NET_SUPPORTING_WM_CHECK")), "xfwm4")
        options = {"stdin": subprocess.PIPE, "stdout": subprocess.PIPE, "stderr": errors, "bufsize": 0}
        def open_control():
            return Control(stack.enter_context(process([args.aad, "control", "serve", "--allow-input",
                                                         "--provider", PROVIDER], **options)), log)

        control = open_control()
        mcp = Mcp(stack.enter_context(process([args.aad, "mcp", "--allow-input"], **options)), log)
        mcp.rpc("initialize", {"protocolVersion": "2024-11-05", "capabilities": {},
                               "clientInfo": {"name": "timed-click-fixture", "version": "1"}})
        medians = {}
        for group, local_x, width in GROUPS:
            with process([args.fixture, group, str(local_x), str(width)], stdin=subprocess.PIPE,
                         stdout=events, stderr=errors, bufsize=0) as child:
                gtk = Gtk(child, gtk_logs)
                center = (40 + local_x, 220)
                assert gtk.ready["box"] == [center[0] - width // 2, 190, width, 60], gtk.ready
                run("xdotool", "windowactivate", "--sync", gtk.xid)
                resource = until(lambda: next((r["resource_id"] for r in control.call("targets",
                    provider=PROVIDER)["resources"] if r["metadata"]["process_id"] == child.pid), None), group)
                locator = {"role": "push_button", "name": group}
                medians[group] = matrix(control, gtk, resource, locator, center, samples, args.sample_count)
                control.target(resource, {"name": f"Activations: {args.sample_count * 3}"}, group)
                if group == "FN":
                    control.call("close")
                    public_and_interruptions(args.aad, open_control, mcp, gtk, resource, locator, center, log)
                gtk.command("quit")
                assert child.wait(timeout=4) == 0
                gtk.stream.close()
        for speed in (1.0, 2.0):
            for small, large in (("NW", "FW"), ("NN", "FN"), ("NW", "NN"), ("FW", "FN")):
                assert medians[small][speed] < medians[large][speed], medians
        print(json.dumps({"x11_motion_medians_ms": medians, "matrix_samples": args.sample_count * 12,
                          "public_entry_points": "CLI, control, MCP, inline, CLI and MCP replay",
                          "changed_target": "resolved by name at new coordinates", "schema_guard": "passed",
                          "interruption": "zero activations; confirmed releases and preserved external button hold",
                          "display": display}, indent=2))


if __name__ == "__main__":
    main()
