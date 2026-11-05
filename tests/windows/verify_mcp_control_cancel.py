#!/usr/bin/env python3
"""Prove pre-worker cancellation with the lifecycle-test-hooks CLI.
Uses invalid synthetic targets and never opens or modifies a browser.
The un-cancelled control proves the same request reaches the endpoint.
"""
import json
import os
import tempfile
import time
from pathlib import Path
from verify_mcp_browser_session import Client

for cancelled in (False, True):
    with tempfile.TemporaryDirectory(prefix="aad-cancel-worker-") as root:
        gate = Path(root) / "release"
        dispatched = Path(root) / "dispatched"
        client = Client(dict(os.environ, AAD_TEST_REQUEST_WORKER_GATE=str(gate),
                             AAD_TEST_CONTROL_DISPATCHED=str(dispatched)))
        def send(value):
            client.process.stdin.write(json.dumps(dict(jsonrpc="2.0", **value)) + "\n")
            client.process.stdin.flush()
        def receive():
            return client.receive(time.monotonic() + 5)
        def calls():
            return [json.loads(line) for line in dispatched.read_text().splitlines()] if dispatched.exists() else []
        try:
            send(dict(id=900, method="tools/call", params=dict(name="control_act", arguments={
                "request_id":"operation", "target":{"session_id":"s","resource_id":"r",
                "observation_id":"o","node_id":"n"}, "action":{"type":"click"}})))
            if cancelled:
                send(dict(method="notifications/cancelled", params=dict(requestId=900)))
            send(dict(id=901, method="ping"))
            assert receive()["id"] == 901
            assert calls() == []
            gate.touch()
            result = receive()
            if cancelled:
                assert result["error"]["code"] == -32800, result
                assert calls() == [], calls()
            else:
                assert "result" in result, result
                assert calls() == ["operation"], calls()
            send(dict(id=900, method="tools/call", params=dict(name="control_targets",
                      arguments=dict(request_id="next-generation"))))
            assert "result" in receive()
            assert calls() == ([] if cancelled else ["operation"]) + ["next-generation"]
            print("PASS control worker", "cancelled" if cancelled else "normal", "and ID reuse")
        finally:
            gate.touch()
            client.close()
