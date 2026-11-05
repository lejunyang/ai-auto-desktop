#!/usr/bin/env python3
"""Deterministic stdio admission regression; requires lifecycle-test-hooks.
No user browser is opened: accepted opens are cancelled before worker dispatch.
"""
import json
import os
import tempfile
import time
from pathlib import Path
from verify_mcp_browser_session import Client

for first, second in [("browser_open", "browser_status"),
                      ("browser_open", "control_close"),
                      ("browser_status", "browser_open"),
                      ("control_close", "browser_open")]:
    with tempfile.TemporaryDirectory() as root:
        gate = Path(root) / "release"
        ack = Path(root) / "cancelled"
        candidate = Path(root) / "candidate"
        client = Client(dict(os.environ, AAD_TEST_REQUEST_WORKER_GATE=str(gate),
                             AAD_TEST_CANCELLED=str(ack), AAD_TEST_CANDIDATE=str(candidate)))
        def send(name):
            client.process.stdin.write(json.dumps({"jsonrpc":"2.0", "id":900,
                "method":"tools/call", "params":{"name":name,"arguments":{}}}) + "\n")
            client.process.stdin.flush()
        try:
            send(first)
            # Repeat rejection to prove a rejected request cannot release the ID.
            for _ in range(2):
                send(second)
                reply = client.receive(time.monotonic()+5)
                assert reply["error"]["code"] == -32600, reply
                assert "duplicate in-flight" in reply["error"]["message"], reply
            if first == "browser_open":
                client.process.stdin.write(json.dumps({"jsonrpc":"2.0",
                    "method":"notifications/cancelled", "params":{"requestId":900}})+"\n")
                client.process.stdin.flush()
                deadline = time.monotonic()+5
                while not ack.exists():
                    assert time.monotonic() < deadline
                    time.sleep(.01)
            gate.touch()
            reply = client.receive(time.monotonic()+10)
            assert "error" not in reply, reply
            if first == "browser_open":
                assert "CONTROL.CANCELLED" in json.dumps(reply), reply
                status, _ = client.tool("browser_status", {})
                assert "SESSION.CLOSED" not in json.dumps(status), status
            assert not candidate.exists(), "duplicate open executed"
            # Completion releases the reservation for reuse, across methods.
            client.process.stdin.write(json.dumps({"jsonrpc":"2.0","id":900,"method":"ping"})+"\n")
            client.process.stdin.flush()
            assert client.receive(time.monotonic()+5)["result"] == {}
            print("PASS", first, "->", second)
        finally:
            gate.touch()
            client.close()
