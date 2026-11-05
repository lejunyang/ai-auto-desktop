"""A live, silent child must not keep the fixture waiting forever."""
import queue
import os
import subprocess
import sys
import threading
import time
import unittest
import tempfile
from pathlib import Path
from unittest.mock import patch
import psutil

from verify_mcp_browser_session import Client


class ClientTimeoutTest(unittest.TestCase):
    def test_orphan_candidate_spawns_stdout_descendant(self):
        self.check_orphan_tree(False)

    def test_orphan_candidate_spawns_devnull_descendant(self):
        self.check_orphan_tree(True)

    def test_candidate_forks_as_cleanup_starts(self):
        self.check_orphan_tree(False, during_cleanup=True)

    def check_orphan_tree(self, devnull, during_cleanup=False):
        # Exercise the real initializer, reader, tracker and candidate log.
        # Only replace the executable; never seed Client.tracked manually.
        with tempfile.TemporaryDirectory() as directory:
            folder = Path(directory)
            script = folder / "fake_mcp.py"
            script.write_text('''import json, os, pathlib, subprocess, sys, time
folder = pathlib.Path(os.environ["TREE_FIXTURE"])
if len(sys.argv) > 1 and sys.argv[1] == "candidate":
    while not (folder / "spawn").exists(): time.sleep(.01)
    child = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"],
                             start_new_session=True,
                             stdout=subprocess.DEVNULL if os.environ["TREE_DEVNULL"] == "1" else None)
    (folder / "grandchild").write_text(str(child.pid))
    time.sleep(120)
else:
    child = subprocess.Popen([sys.executable, __file__, "candidate"], start_new_session=True)
    pathlib.Path(os.environ["AAD_TEST_CANDIDATE_LOG"]).write_text(str(child.pid) + "\\n")
    (folder / "candidate").write_text(str(child.pid))
    request = json.loads(sys.stdin.readline())
    print(json.dumps({"jsonrpc":"2.0", "id":request["id"], "result":{}}), flush=True)
''')
            unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"])
            original_popen = subprocess.Popen
            def launch(command, **kwargs):
                return original_popen([sys.executable, str(script)], **kwargs)
            client = None
            descendants = []
            try:
                with patch("verify_mcp_browser_session.subprocess.Popen", side_effect=launch):
                    client = Client(dict(os.environ, TREE_FIXTURE=directory,
                                         TREE_DEVNULL="1" if devnull else "0"))
                client.process.wait(timeout=3)
                # The candidate forks only AFTER the MCP parent has exited.
                candidate_pid = int((folder / "candidate").read_text())
                def spawn_descendant():
                    (folder / "spawn").touch()
                    deadline = time.monotonic() + 3
                    while not (folder / "grandchild").exists():
                        self.assertLess(time.monotonic(), deadline)
                        time.sleep(.01)
                    descendants.extend(psutil.Process(int((folder / name).read_text()))
                                       for name in ("candidate", "grandchild"))
                if not during_cleanup:
                    spawn_descendant()
                original_suspend = psutil.Process.suspend
                def suspend(process):
                    if during_cleanup and process.pid == candidate_pid:
                        # Fork after tracker has stopped, just before the
                        # cleanup traversal freezes this candidate root.
                        spawn_descendant()
                    original_suspend(process)
                started = time.monotonic()
                with patch.object(psutil.Process, "suspend", suspend):
                    client.close()
                self.assertLess(time.monotonic() - started, 7)
                self.assertFalse(client.reader.is_alive())
                for process in descendants:
                    self.assertTrue(not process.is_running() or process.status() == psutil.STATUS_ZOMBIE)
                self.assertIsNone(unrelated.poll())
            finally:
                # Independent supervisor cleanup even when assertions fail.
                for name in ("grandchild", "candidate"):
                    try:
                        process = psutil.Process(int((folder / name).read_text()))
                        process.kill()
                    except (FileNotFoundError, psutil.NoSuchProcess):
                        pass
                unrelated.kill()
                unrelated.wait(timeout=3)

    def test_silent_process_times_out_and_is_reaped(self):
        self.check_cleanup(False)

    def test_exited_parent_with_live_stdout_child_is_reaped(self):
        self.check_cleanup(True)

    def test_independent_candidate_with_devnull_is_reaped(self):
        self.check_cleanup(False, True)

    def check_cleanup(self, parent_exits, devnull=False):
        client = Client.__new__(Client)
        client.process = subprocess.Popen(
            [sys.executable, "-c", "import subprocess,sys,time; "
             "child=subprocess.Popen([sys.executable,'-c','import time; time.sleep(120)'],start_new_session=True,stdout=subprocess.DEVNULL if sys.argv[2]=='null' else None); "
             "print(child.pid,flush=True); time.sleep(0.2 if sys.argv[1]=='exit' else 120)",
             "exit" if parent_exits else "wait", "null" if devnull else "pipe"],
            stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True,
            start_new_session=os.name != "nt",
        )
        child_pid = int(client.process.stdout.readline())
        child = psutil.Process(child_pid)
        client.tracked = {(child.pid, child.create_time()): child}
        client.tracking_stop = threading.Event()
        client.tracker = threading.Thread(target=lambda: None)
        client.tracker.start()
        unrelated = subprocess.Popen([sys.executable, "-c", "import time; time.sleep(120)"])
        client.responses = queue.Queue()
        client.reader = threading.Thread(
            target=lambda: client.responses.put(client.process.stdout.readline()),
            daemon=True,
        )
        client.reader.start()
        if parent_exits:
            client.process.wait(timeout=2)
        started = time.monotonic()
        try:
            with self.assertRaises(TimeoutError):
                client.receive(started + 0.1)
            if not parent_exits:
                self.assertIsNone(client.process.poll())
        finally:
            try:
                client.close()
                self.assertIsNone(unrelated.poll(), "cleanup touched an unrelated process")
            finally:
                unrelated.kill()
                unrelated.wait(timeout=5)
        self.assertLess(time.monotonic() - started, 12)
        self.assertIsNotNone(client.process.poll())
        self.assertFalse(client.reader.is_alive())
        self.assertTrue(not child.is_running() or child.status()==psutil.STATUS_ZOMBIE)
        # A surviving descendant would retain the inherited stdout pipe and
        # keep the reader blocked even after its parent was killed.
        self.assertEqual(client.responses.get_nowait(), "", f"descendant {child_pid} retained stdout")


if __name__ == "__main__":
    unittest.main()
