"""Isolated loopback regression: failed replacement preserves an external session.

Build: cargo build -p aad-cli --features aad-mcp/lifecycle-test-hooks
Test dependencies: python3 -m pip install -r tests/windows/requirements.txt
Run: $env:AAD_TEST_BROWSER='edge'; python3 tests/windows/verify_browser_open_atomic.py
The opt-in test feature exposes lifecycle contention and candidate identity;
ordinary builds contain neither the probes nor attachment failure injection.
"""
import os
import json
import tempfile
import threading
import time
import socket
from pathlib import Path
import verify_mcp_browser_session as fixture
from verify_mcp_browser_session import Client, Site, observe, node
from http.server import ThreadingHTTPServer

entered = threading.Event()
release = threading.Event()
successful = False


def process_exists(pid):
    """Read-only process query; Windows kill(pid, 0) would terminate it."""
    if os.name != "nt":
        try:
            os.kill(pid, 0)
        except ProcessLookupError:
            return False
        except PermissionError:
            return True
        return True
    import ctypes
    from ctypes import wintypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.OpenProcess.argtypes = [wintypes.DWORD, wintypes.BOOL, wintypes.DWORD]
    kernel.OpenProcess.restype = wintypes.HANDLE
    kernel.GetExitCodeProcess.argtypes = [wintypes.HANDLE, ctypes.POINTER(wintypes.DWORD)]
    kernel.GetExitCodeProcess.restype = wintypes.BOOL
    kernel.CloseHandle.argtypes = [wintypes.HANDLE]
    kernel.CloseHandle.restype = wintypes.BOOL
    handle = kernel.OpenProcess(0x1000, False, pid)
    if not handle:
        error = ctypes.get_last_error()
        if error == 87:  # ERROR_INVALID_PARAMETER: PID no longer exists.
            return False
        if error == 5:  # Access denied still establishes existence.
            return True
        raise ctypes.WinError(error)
    try:
        code = wintypes.DWORD()
        if not kernel.GetExitCodeProcess(handle, ctypes.byref(code)):
            raise ctypes.WinError(ctypes.get_last_error())
        return code.value == 259  # STILL_ACTIVE
    finally:
        kernel.CloseHandle(handle)


class HeldSite(Site):
    def do_GET(self):
        if self.path == "/held":
            entered.set()
            assert release.wait(30), "test did not release navigation"
            if successful:
                self.path = "/login"
                return super().do_GET()
            self.close_connection = True
            return
        super().do_GET()


def concurrent_open(env, port, browser, site, both_succeed=False):
    global successful
    successful = both_succeed
    entered.clear()
    release.clear()
    events = os.path.join(env["AAD_RECORDINGS_DIR"], "lifecycle-events")
    if os.path.exists(events):
        os.unlink(events)
    client = Client(dict(env, AAD_TEST_LIFECYCLE=events), port)
    try:
        old = observe(client, "Fixture login")
        old_target = node(old, lambda n: n["name"] == "Sign in")["target"]
        # Write both requests without the synchronous Client response reader.
        def send(identifier, args):
            client.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier,
                "method": "tools/call", "params": {"name": "browser_open", "arguments":
                {"browser": browser, "headless": True, **args}}}) + "\n")
            client.process.stdin.flush()
        send(100, {"url": site + "/held"})
        assert entered.wait(20), "candidate never reached held navigation"
        for identifier in range(200, 207):
            client.process.stdin.write(json.dumps({"jsonrpc": "2.0", "id": identifier,
                "method": "tools/call", "params": {"name": "browser_status", "arguments": {}}}) + "\n")
        client.process.stdin.flush()
        deadline = time.monotonic() + 5
        for _ in range(7):
            answer = client.receive(deadline)
            assert answer.get("id") in range(200, 207), answer
            assert "BROWSER.BUSY" in json.dumps(answer), answer
        send(101, {"url": site + "/login", "record": {}})
        deadline = time.monotonic() + 10
        while True:
            with open(events) as f:
                stages = f.read().splitlines()
            if len(stages) == 2:
                break
            assert time.monotonic() < deadline, "B never reached lifecycle lock"
            time.sleep(0.01)
        assert stages == ["false", "true"], stages
        release.set()
        answers = {}
        response_deadline = time.monotonic() + 60
        while len(answers) < 2:
            answer = client.receive(response_deadline)
            if answer.get("id") in (100, 101):
                answers[answer["id"]] = answer["result"]
        if both_succeed:
            assert not answers[100].get("isError"), answers
            assert "BROWSER.ALREADY_OPEN" in json.dumps(answers[101]), answers
        else:
            assert answers[100].get("isError"), answers
            assert not answers[101].get("isError"), answers
        winner = answers[100 if both_succeed else 101]
        opened = json.loads(next(c["text"] for c in winner["content"] if c["type"] == "text"))
        status, _ = client.tool("browser_status", {})
        assert status["port"] == opened["port"], status
        assert process_exists(status["process_id"]), status
        stale = client.request("tools/call", {"name": "control_capture", "arguments": {"target": old_target}})
        assert "CONTROL.STALE_TARGET" in json.dumps(stale), stale
        current = observe(client, "Fixture login")
        downloads, _ = client.tool("browser_downloads", {})
        assert downloads["tracking"], downloads
        if not both_succeed:
            target = node(current, lambda n: n["name"] == "Sign in")["target"]
            client.tool("control_act", {"target": target, "action": {"type": "click"}})
            time.sleep(1)
            stopped, _ = client.tool("browser_record_stop", {})
            assert os.path.isdir(stopped["out"]), stopped
            with open(os.path.join(stopped["out"], "evidence", "network.jsonl")) as f:
                assert "/after-click" in f.read(), "recording lost subsequent request"
    finally:
        release.set()
        client.close()


def main():
    # The same check runs on Windows and POSIX: repeated probes must leave a
    # child alive and able to complete normally, then report it absent.
    import subprocess
    import sys
    child = subprocess.Popen([sys.executable, "-c", "import sys; sys.stdin.read(); print('alive')"],
                             stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    try:
        for _ in range(10):
            assert process_exists(child.pid)
            assert child.poll() is None, "liveness query terminated the child"
        output, _ = child.communicate(timeout=10)
        assert output.strip() == "alive" and child.returncode == 0
        assert not process_exists(child.pid)
    finally:
        if child.poll() is None:
            child.terminate()
            child.wait(timeout=10)
    fixture.LOGIN = fixture.LOGIN.replace("location.href='/home'",
        "document.querySelector('h1').textContent='Clicked '+Date.now();fetch('/after-click')")
    browser = os.environ.get("AAD_TEST_BROWSER", "chrome")
    with tempfile.TemporaryDirectory(prefix="aad-atomic-") as root:
        env = dict(os.environ, AAD_RECORDINGS_DIR=root, AAD_CONTROL_LEASE_DIR=os.path.join(root,"leases"))
        server = ThreadingHTTPServer(("127.0.0.1", 0), HeldSite)
        threading.Thread(target=server.serve_forever, daemon=True).start()
        owner = Client(env)
        try:
            opened, _ = owner.tool("browser_open", {
                "browser": browser, "headless": True,
                "url": f"http://127.0.0.1:{server.server_port}/login"})
            blocked = os.path.join(root, "file")
            with open(blocked, "w") as f:
                f.write("not a directory")
            for extra, args, code in [
                ({}, {"url": "http://"}, "MCP.INVALID_ARGUMENT"),
                ({}, {"profile_dir": blocked}, "BROWSER.START_FAILED"),
                ({"AAD_RECORDINGS_DIR": blocked}, {"record": {}}, "BROWSER.FAILED"),
                ({}, {"record": {"body_limit": 1048577}}, "BROWSER.FAILED"),
                ({}, {"profile_dir": opened["profile_dir"], "close_running": True,
                      "record": {"body_limit": 1048577}}, "BROWSER.FAILED"),
                ({}, {"profile_dir": opened["profile_dir"], "close_running": True}, "BROWSER.UNSAFE_REPLACEMENT"),
                ({"AAD_TEST_ATTACH_FAILURE": "1"}, {"record": {}}, "BROWSER.FAILED"),
                ({"AAD_TEST_RECORDER_FAILURE": "1"}, {"record": {}}, "BROWSER.FAILED"),
                ({}, {"url": "http://127.0.0.1:1/", "record": {}}, "BROWSER.FAILED"),
            ]:
                candidate = os.path.join(root, "candidate")
                if os.path.exists(candidate):
                    os.unlink(candidate)
                client = Client(dict(env, AAD_TEST_CANDIDATE=candidate, **extra), opened["port"])
                try:
                    old = observe(client, "Fixture login")
                    target = node(old, lambda n: n["name"] == "Sign in")["target"]
                    before, _ = client.tool("browser_status", {})
                    result = client.request("tools/call", {
                        "name": "browser_open", "arguments": {
                            "browser": browser, "headless": True, **args}})["result"]
                    assert result.get("isError"), result
                    assert code in json.dumps(result), result
                    after, _ = client.tool("browser_status", {})
                    assert after["port"] == before["port"] == opened["port"], after
                    assert after["ownership"] == before["ownership"], after
                    client.tool("control_find", {"observation_id": target["observation_id"],
                                                "locator": {"name": "Sign in"}})
                    client.tool("control_capture", {"target": target})
                    client.tool("control_act", {"target": target, "action": {"type": "click"}})
                    effect = observe(client, "Fixture login")
                    assert any(n["name"].startswith("Clicked ") for n in effect["nodes"]), effect
                    if os.path.exists(candidate):
                        with open(candidate) as f:
                            pid, candidate_port = map(int, f.read().split())
                        assert not process_exists(pid), f"candidate {pid} still alive"
                        with socket.socket() as sock:
                            sock.settimeout(1)
                            assert sock.connect_ex(("127.0.0.1", candidate_port)) != 0
                    stopped = client.request("tools/call", {"name": "browser_record_stop", "arguments": {}})
                    assert "BROWSER.NOT_RECORDING" in json.dumps(stopped), stopped
                    for manifest in Path(root).rglob("manifest.json"):
                        with manifest.open() as f:
                            assert json.load(f).get("status") != "recording", str(manifest)
                finally:
                    client.close()
            concurrent_open(env, opened["port"], browser,
                            f"http://127.0.0.1:{server.server_port}")
            concurrent_open(env, opened["port"], browser,
                            f"http://127.0.0.1:{server.server_port}", True)
            global successful
            # Input reader registers identity before the open worker may run.
            gate = os.path.join(root, "open-worker-release")
            early_ack = os.path.join(root, "early-cancel-ack")
            early_candidate = os.path.join(root, "early-candidate")
            client = Client(dict(env, AAD_TEST_OPEN_WORKER_GATE=gate,
                                 AAD_TEST_CANCELLED=early_ack,
                                 AAD_TEST_CANDIDATE=early_candidate), opened["port"])
            try:
                old = observe(client, "Fixture login")
                target = node(old, lambda n: n["name"] == "Sign in")["target"]
                frames = [
                    {"jsonrpc":"2.0","id":899,"method":"tools/call","params":{
                        "name":"browser_open","arguments":{"browser":browser,"headless":True}}},
                    *[{"jsonrpc":"2.0","id":899,"method":"tools/call","params":{
                        "name":name,"arguments":{}}} for name in ("browser_status", "control_close")],
                    {"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":899}},
                ]
                client.process.stdin.write("".join(json.dumps(f)+"\n" for f in frames))
                client.process.stdin.flush()
                for _ in range(2):
                    duplicate = client.receive(time.monotonic()+5)
                    assert duplicate["error"]["code"] == -32600, duplicate
                    assert "duplicate in-flight" in duplicate["error"]["message"]
                deadline = time.monotonic()+5
                while not Path(early_ack).exists():
                    assert time.monotonic() < deadline
                    time.sleep(.01)
                Path(gate).touch()
                reply = client.receive(time.monotonic()+10)
                assert "CONTROL.CANCELLED" in json.dumps(reply), reply
                assert not Path(early_candidate).exists()
                status, _ = client.tool("browser_status", {})
                assert status["port"] == opened["port"], status
                client.tool("control_capture", {"target":target})
            finally:
                Path(gate).touch()
                client.close()
            # Cancellation wins before successful navigation publishes its candidate.
            successful = True
            entered.clear()
            release.clear()
            candidate = os.path.join(root, "cancel-candidate")
            cancelled = os.path.join(root, "cancel-ack")
            client = Client(dict(env, AAD_TEST_CANDIDATE=candidate,
                                 AAD_TEST_CANCELLED=cancelled), opened["port"])
            try:
                old = observe(client, "Fixture login")
                target = node(old, lambda n: n["name"] == "Sign in")["target"]
                client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":900,
                    "method":"tools/call","params":{"name":"browser_open","arguments":{
                    "browser":browser,"headless":True,
                    "url":f"http://127.0.0.1:{server.server_port}/held"}}})+"\n")
                client.process.stdin.flush()
                assert entered.wait(20)
                # Cancel all seven queued opens while A retains its page barrier.
                for queued in range(910, 917):
                    client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":queued,
                        "method":"tools/call","params":{"name":"browser_open","arguments":{
                        "browser":browser,"headless":True}}})+"\n")
                for queued in range(910, 917):
                    client.process.stdin.write(json.dumps({"jsonrpc":"2.0",
                        "method":"notifications/cancelled","params":{"requestId":queued}})+"\n")
                client.process.stdin.flush()
                queued_replies = [client.receive(time.monotonic()+5) for _ in range(7)]
                assert {r["id"] for r in queued_replies} == set(range(910,917)), queued_replies
                assert all("CONTROL.CANCELLED" in json.dumps(r) for r in queued_replies)
                assert client.request("ping", {})["result"] == {}

                client.process.stdin.write(json.dumps({"jsonrpc":"2.0",
                    "method":"notifications/cancelled","params":{"requestId":900}})+"\n")
                client.process.stdin.flush()
                deadline = time.monotonic() + 5
                while not Path(cancelled).exists():
                    assert time.monotonic() < deadline, "cancellation not registered"
                    time.sleep(.01)
                release.set()
                reply = client.receive(time.monotonic()+20)
                assert reply["id"] == 900 and "CONTROL.CANCELLED" in json.dumps(reply), reply
                assert json.loads(reply["result"]["content"][0]["text"])["effect"] == "unknown", reply
                status, _ = client.tool("browser_status", {})
                assert status["port"] == opened["port"] and status["ownership"] == "external", status
                client.tool("control_capture", {"target":target})
                pid, port = map(int, Path(candidate).read_text().split())
                assert not process_exists(pid), pid
                with socket.socket() as sock:
                    assert sock.connect_ex(("127.0.0.1", port)) != 0
            finally:
                release.set()
                client.close()
            # Cancellation interrupts recorder startup without releasing its barrier.
            successful = True
            entered.clear()
            release.clear()
            candidate = os.path.join(root, "recorder-candidate")
            cancelled = os.path.join(root, "recorder-cancel-ack")
            client = Client(dict(env, AAD_TEST_CANDIDATE=candidate,
                                 AAD_TEST_CANCELLED=cancelled,
                                 AAD_TEST_RECORDER_START_BARRIER=os.path.join(root,"recorder-barrier")), opened["port"])
            try:
                old = observe(client, "Fixture login")
                target = node(old, lambda n: n["name"] == "Sign in")["target"]
                client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":900,
                    "method":"tools/call","params":{"name":"browser_open","arguments":{
                    "browser":browser,"headless":True,"record":{}}}})+"\n")
                client.process.stdin.flush()
                deadline = time.monotonic()+20
                while not Path(root, "recorder-barrier").exists():
                    assert time.monotonic() < deadline
                    time.sleep(.01)
                client.process.stdin.write(json.dumps({"jsonrpc":"2.0",
                    "method":"notifications/cancelled","params":{"requestId":900}})+"\n")
                client.process.stdin.flush()
                deadline = time.monotonic() + 5
                while not Path(cancelled).exists():
                    assert time.monotonic() < deadline, "cancellation not registered"
                    time.sleep(.01)
                reply = client.receive(time.monotonic()+20)
                assert reply["id"] == 900 and "CONTROL.CANCELLED" in json.dumps(reply), reply
                assert json.loads(reply["result"]["content"][0]["text"])["effect"] == "not_applied", reply
                status, _ = client.tool("browser_status", {})
                assert status["port"] == opened["port"] and status["ownership"] == "external", status
                client.tool("control_capture", {"target":target})
                pid, port = map(int, Path(candidate).read_text().split())
                assert not process_exists(pid), pid
                stopped = client.request("tools/call", {"name":"browser_record_stop","arguments":{}})
                assert "BROWSER.NOT_RECORDING" in json.dumps(stopped), stopped
                with socket.socket() as sock:
                    assert sock.connect_ex(("127.0.0.1", port)) != 0
            finally:
                release.set()
                client.close()
            # Cancellation interrupts prenav startup without releasing its barrier.
            successful = True
            entered.clear()
            release.clear()
            candidate = os.path.join(root, "prenav-candidate")
            cancelled = os.path.join(root, "prenav-cancel-ack")
            client = Client(dict(env, AAD_TEST_CANDIDATE=candidate,
                                 AAD_TEST_CANCELLED=cancelled,
                                 AAD_TEST_BEFORE_NAVIGATION=os.path.join(root,"prenav-barrier")), opened["port"])
            try:
                old = observe(client, "Fixture login")
                target = node(old, lambda n: n["name"] == "Sign in")["target"]
                client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":900,
                    "method":"tools/call","params":{"name":"browser_open","arguments":{
                    "browser":browser,"headless":True,"url":f"http://127.0.0.1:{server.server_port}/held"}}})+"\n")
                client.process.stdin.flush()
                deadline = time.monotonic()+20
                while not Path(root, "prenav-barrier").exists():
                    assert time.monotonic() < deadline
                    time.sleep(.01)
                client.process.stdin.write(json.dumps({"jsonrpc":"2.0",
                    "method":"notifications/cancelled","params":{"requestId":900}})+"\n")
                client.process.stdin.flush()
                deadline = time.monotonic() + 5
                while not Path(cancelled).exists():
                    assert time.monotonic() < deadline, "cancellation not registered"
                    time.sleep(.01)
                reply = client.receive(time.monotonic()+20)
                assert reply["id"] == 900 and reply["result"].get("isError"), reply
                assert not entered.is_set(), "cancelled candidate navigated"
                status, _ = client.tool("browser_status", {})
                assert status["port"] == opened["port"] and status["ownership"] == "external", status
                client.tool("control_capture", {"target":target})
                pid, port = map(int, Path(candidate).read_text().split())
                assert not process_exists(pid), pid
                stopped = client.request("tools/call", {"name":"browser_record_stop","arguments":{}})
                assert "BROWSER.NOT_RECORDING" in json.dumps(stopped), stopped
                with socket.socket() as sock:
                    assert sock.connect_ex(("127.0.0.1", port)) != 0
            finally:
                release.set()
                client.close()
            for record_index, record_options in enumerate([{}, {"record": {}}]):
                # Closing alone revokes pre-navigation candidates.
                successful = True
                entered.clear()
                release.clear()
                candidate = os.path.join(root, f"close-{record_index}-prenav-candidate")
                cancelled = os.path.join(root, f"close-{record_index}-prenav-cancel-ack")
                client = Client(dict(env, AAD_TEST_CANDIDATE=candidate,
                                     AAD_TEST_CANCELLED=cancelled,
                                     AAD_TEST_BEFORE_NAVIGATION=os.path.join(root,f"close-{record_index}-prenav-barrier")), opened["port"])
                try:
                    old = observe(client, "Fixture login")
                    target = node(old, lambda n: n["name"] == "Sign in")["target"]
                    client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":900,
                        "method":"tools/call","params":{"name":"browser_open","arguments":{
                        "browser":browser,"headless":True,"url":f"http://127.0.0.1:{server.server_port}/held", **record_options}}})+"\n")
                    client.process.stdin.flush()
                    deadline = time.monotonic()+20
                    while not Path(root, f"close-{record_index}-prenav-barrier").exists():
                        assert time.monotonic() < deadline
                        time.sleep(.01)
                    client.process.stdin.write(json.dumps({"jsonrpc":"2.0",
                        "id":901,"method":"tools/call","params":{"name":"control_close","arguments":{}}})+"\n")
                    client.process.stdin.flush()
                    replies = [client.receive(time.monotonic()+20) for _ in range(2)]
                    closed = next(r for r in replies if r["id"] == 901)
                    assert "true" in json.dumps(closed), closed
                    reply = next(r for r in replies if r["id"] == 900)
                    assert reply["result"].get("isError"), reply
                    assert not entered.is_set(), "cancelled candidate navigated"
                    pid, port = map(int, Path(candidate).read_text().split())
                    assert not process_exists(pid), pid
                    with socket.socket() as sock:
                        assert sock.connect_ex(("127.0.0.1", port)) != 0
                finally:
                    release.set()
                    client.close()
            # A held lifecycle effect must not block reading cancellation/ping/close.
            gate = os.path.join(root, "effect-gate")
            client = Client(dict(env, AAD_TEST_EFFECT_GATE=gate), opened["port"])
            try:
                client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":910,"method":"tools/call",
                    "params":{"name":"browser_open","arguments":{"browser":browser,"headless":True}}})+"\n")
                client.process.stdin.flush()
                deadline=time.monotonic()+5
                while not Path(gate).exists():
                    assert time.monotonic()<deadline
                    time.sleep(.01)
                frames=[
                    {"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":910}},
                    {"jsonrpc":"2.0","id":911,"method":"tools/call","params":{"name":"control_close","arguments":{}}},
                    {"jsonrpc":"2.0","id":912,"method":"ping"},
                ]
                client.process.stdin.write("".join(json.dumps(f)+"\n" for f in frames))
                client.process.stdin.flush()
                reply=client.receive(time.monotonic()+5)
                assert reply["id"]==912, reply
                Path(gate+".release").touch()
                answers=[client.receive(time.monotonic()+10) for _ in range(2)]
                assert {r["id"] for r in answers}=={910,911}, answers
                assert next(r for r in answers if r["id"]==910)["result"].get("isError"), answers
                assert not next(r for r in answers if r["id"]==911)["result"].get("isError"), answers
            finally:
                Path(gate+".release").touch()
                client.close()
            # Close wins while the candidate is preparing, before publication.
            successful = True
            entered.clear()
            release.clear()
            candidate = os.path.join(root, "closing-candidate")
            events=os.path.join(root,"queued-close-events")
            client = Client(dict(env, AAD_TEST_CANDIDATE=candidate,AAD_TEST_LIFECYCLE=events), opened["port"])
            try:
                client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":100,
                    "method":"tools/call","params":{"name":"browser_open","arguments":{
                    "browser":browser,"headless":True,"record":{},
                    "url":f"http://127.0.0.1:{server.server_port}/held"}}})+"\n")
                client.process.stdin.flush()
                assert entered.wait(20)
                for identifier in range(300, 307):
                    client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":identifier,
                        "method":"tools/call","params":{"name":"browser_status","arguments":{}}})+"\n")
                client.process.stdin.flush()
                deadline = time.monotonic() + 5
                for _ in range(7):
                    reply = client.receive(deadline)
                    assert "BROWSER.BUSY" in json.dumps(reply), reply
                client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":101,"method":"tools/call",
                    "params":{"name":"browser_open","arguments":{"browser":browser,"headless":True,
                    "profile_dir":opened["profile_dir"],"close_running":True}}})+"\n")
                client.process.stdin.flush()
                deadline=time.monotonic()+5
                while len(Path(events).read_text().splitlines())<2:
                    assert time.monotonic()<deadline
                    time.sleep(.01)
                client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":400,
                    "method":"tools/call","params":{"name":"control_close","arguments":{}}})+"\n")
                client.process.stdin.flush()
                answers = []
                while True:
                    reply = client.receive(time.monotonic() + 5)
                    if reply["id"] == 400:
                        assert not reply["result"].get("isError"), reply
                        break
                    answers.append(reply)
                release.set()
                answers += [client.receive(time.monotonic()+15) for _ in range(2-len(answers))]
                assert {a["id"] for a in answers}=={100,101},answers
                assert all(any(code in json.dumps(a) for code in ["CONTROL.SESSION_CLOSED","CONTROL.CANCELLED"]) for a in answers),answers
                pid, candidate_port = map(int, Path(candidate).read_text().split())
                assert not process_exists(pid),pid
                with socket.socket() as sock:
                    sock.settimeout(1)
                    assert sock.connect_ex(("127.0.0.1",candidate_port)) != 0
                stopped=client.request("tools/call",{"name":"browser_record_stop","arguments":{}})
                assert "BROWSER.NOT_RECORDING" in json.dumps(stopped),stopped
                observe(owner,"Fixture login")
            finally:
                release.set()
                client.close()
        finally:
            owner.close()
            server.shutdown()
    print("PASS: invalid URL, launch, recording and navigation failures preserve external session")


if __name__ == "__main__":
    main()
