"""Run under a private dbus-run-session and xvfb-run, using system PyGObject."""
import json
import select
import subprocess
import sys
import time


def application():
    import gi
    gi.require_version("Gtk", "3.0")
    gi.require_version("Atk", "1.0")
    from gi.repository import Atk, GLib, Gtk

    GLib.set_prgname("aad-grid-fixture")
    window = Gtk.Window(title="Unsaved ledger")
    model = Gtk.ListStore(str, str, str)
    model.append(["INV-01", "old", "unrequested"])
    model.append(["INV-02", "19.00", "x" * 600])
    grid = Gtk.TreeView(model=model)
    grid.get_accessible().set_name("Unsaved grid")
    for index, label in enumerate(["ID", "Amount", "Notes"]):
        grid.append_column(Gtk.TreeViewColumn(label, Gtk.CellRendererText(), text=index))
    window.add(grid)
    window.show_all()
    protected_cell = grid.get_accessible().ref_at(1, 2)

    def receive(_stream, _condition):
        command = sys.stdin.readline().strip()
        if command == "change":
            model[0][1] = "73.25"
        elif command == "protect":
            protected_cell.set_role(Atk.Role.PASSWORD_TEXT)
            model[1][2] = "private fixture sentinel"
        elif command == "duplicate":
            grid.get_column(2).set_title("Amount")
        elif command == "rename":
            grid.get_accessible().set_name("Different grid")
        print(json.dumps([{"ID": row[0], "Amount": row[1]} for row in model]), flush=True)
        return True

    GLib.io_add_watch(sys.stdin, GLib.IO_IN, receive)
    Gtk.main()


def verify(aad):
    app = subprocess.Popen([sys.executable, __file__, "--app"], stdin=subprocess.PIPE,
                           stdout=subprocess.PIPE, text=True)
    mcp = subprocess.Popen([aad, "mcp", "--provider", "desktop.linux_atspi"],
                           stdin=subprocess.PIPE, stdout=subprocess.PIPE, text=True)
    def exchange(process, message):
        print(message, file=process.stdin, flush=True)
        assert select.select([process.stdout], [], [], 20)[0], "fixture timeout"
        return json.loads(process.stdout.readline())

    def call(name, args, expect_error=False):
        response = exchange(mcp, json.dumps({"jsonrpc":"2.0", "id":next(ids),
            "method":"tools/call", "params":{"name":name, "arguments":args}}))["result"]
        answer = json.loads(response["content"][0]["text"])
        assert response["isError"] == expect_error, answer
        return answer if expect_error else answer["data"]

    def observe():
        observed = call("control_observe", {"resource_id":resource, "view":{"mode":"nodes"}})
        return call("control_find", {"observation_id":observed["observation_id"],
            "locator":{"name":"Unsaved grid"}})["target"]

    ids = iter(range(1, 1000))
    try:
        until = time.monotonic() + 10
        while True:
            resource = next((item["resource_id"] for item in call("control_targets", {})["resources"]
                             if item["metadata"].get("process_id") == app.pid), None)
            if resource:
                break
            assert time.monotonic() < until, "GTK not registered on private AT-SPI bus"
            time.sleep(0.1)
        target = observe()
        expected = exchange(app, "change")
        answer = call("control_read_table", {"target":target, "columns":["Amount","ID"]})
        assert answer["records"] == expected and not answer["truncated"], answer
        limited = call("control_read_table", {"target":observe(), "columns":["Amount", "Notes"],"max_characters":500})
        assert limited["truncation_reasons"] == ["max_characters"] and limited["truncated"], limited
        assert limited["records"] == [{"Amount":"73.25", "Notes":"unrequested"}], limited
        target = observe()
        wrong = call("control_read_table", {"target":{**target, "resource_id":"ungranted"},
                     "columns":["Amount"]}, True)
        assert wrong["error"]["code"] == "POLICY.DENIED", wrong
        missing = call("control_read_table", {"target":target, "columns":["Unknown"]}, True)
        assert missing["error"]["code"] == "DRIVER.ACTION_UNSUPPORTED", missing
        exchange(app, "protect")
        protected = call("control_read_table", {"target":observe(), "columns":["Notes"]}, True)
        assert protected["error"]["code"] == "DRIVER.ACTION_UNSUPPORTED", protected
        assert "private fixture sentinel" not in json.dumps(protected)
        public = call("control_read_table", {"target":observe(), "columns":["Amount","ID"]})
        assert public["records"] == expected, public
        exchange(app, "duplicate")
        duplicate = call("control_read_table", {"target":observe(), "columns":["Amount"]}, True)
        assert "ambiguous" in duplicate["error"]["message"], duplicate
        target = observe()
        exchange(app, "rename")
        stale = call("control_read_table", {"target":target, "columns":["Amount"]}, True)
        assert stale["error"]["code"] == "DRIVER.STALE_SNAPSHOT", stale
        print(json.dumps(answer))
    finally:
        for process in (mcp, app):
            process.terminate()
            process.wait(timeout=5)


if __name__ == "__main__":
    if sys.argv[1] == "--app":
        application()
    else:
        verify(sys.argv[1])
