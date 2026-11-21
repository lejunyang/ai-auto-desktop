"""Exercise normal CLI actions and independently inspect the target process's UI events."""
import json
import os
from pathlib import Path
import select
import subprocess
import sys
import tempfile

from wayland_form import wait_for


def application(receipt):
    from PyQt5.QtCore import QEvent, QObject, Qt, QTimer
    from PyQt5.QtWidgets import QApplication, QLabel, QLineEdit, QPushButton, QScrollBar, QVBoxLayout, QWidget

    app = QApplication(sys.argv[:1])
    assert app.platformName().startswith("wayland"), app.platformName()
    other, main = QWidget(), QWidget()
    main.setWindowTitle("Invoice")
    other.setWindowTitle("Invoice")
    other_box, box = QVBoxLayout(other), QVBoxLayout(main)
    sibling = QLineEdit("unchanged")
    field = QLineEdit("INV-2037")
    button = QPushButton("Submit invoice")
    covered = QPushButton("Covered action")
    cover = QLabel("Cover", covered)
    secret = QLineEdit()
    bar = QScrollBar(Qt.Horizontal)
    bar.setAccessibleName("Invoice scroll")
    bar.setRange(0, 1000)
    bar.setValue(500)
    bar.setMinimumSize(240, 32)
    edge_cover = QLabel("Blocked", bar)
    edge_cover.setGeometry(0, 0, 40, 32)
    secret.setEchoMode(QLineEdit.Password)
    for widget, name in [(field, "Invoice number"), (sibling, "Other invoice"), (secret, "PIN")]:
        widget.setAccessibleName(name)
    other_box.addWidget(sibling)
    for widget in (field, button, covered, secret, bar):
        box.addWidget(widget)
    events = []
    clicks = []
    gestures = []
    mouse = []
    class MouseSurface(QWidget):
        def event(self, event):
            names = {QEvent.MouseButtonPress: "press", QEvent.MouseButtonRelease: "release",
                     QEvent.MouseButtonDblClick: "double", QEvent.MouseMove: "move"}
            if event.type() in names:
                mouse.append(dict(phase=names[event.type()], button=int(event.button()),
                                  held=int(event.buttons()), modifiers=int(event.modifiers()),
                                  pos=[event.pos().x(), event.pos().y()], size=[self.width(), self.height()]))
                save()
                event.accept()
                return True
            return super().event(event)

    surface = MouseSurface()
    surface.setAccessibleName("Mouse pad")
    surface.setMinimumSize(240, 48)
    box.addWidget(surface)
    pad_cover = QLabel("Blocked", surface)
    pad_cover.setGeometry(0, 0, 36, 48)
    def save():
        tmp = receipt.with_suffix(".tmp")
        tmp.write_text(json.dumps(dict(events=events, clicks=clicks, text=field.text(),
                                      selection=field.selectedText(), sibling=sibling.text(),
                                      platform=app.platformName(), gestures=gestures, scroll_value=bar.value(),
                                      mouse=mouse)))
        tmp.replace(receipt)

    class Monitor(QObject):
        def eventFilter(self, obj, event):
            if not event.spontaneous() and event.type() in (QEvent.Wheel, QEvent.HoverMove):
                entry = dict(widget=obj.accessibleName() or obj.objectName(), pos=[event.pos().x(), event.pos().y()],
                             size=[obj.width(), obj.height()])
                if event.type() == QEvent.Wheel:
                    entry["wheel"] = [event.angleDelta().x(), event.angleDelta().y()]
                else:
                    entry["hover"] = True
                gestures.append(entry)
                QTimer.singleShot(0, save)
            kind = {QEvent.KeyPress: "key-down", QEvent.KeyRelease: "key-up",
                    QEvent.MouseButtonPress: "mouse-down", QEvent.MouseButtonRelease: "mouse-up"}
            if event.type() in kind:
                events.append([obj.accessibleName() or obj.text(), kind[event.type()]])
                QTimer.singleShot(0, save)
            return False

    monitor = Monitor()
    for widget in (sibling, field, button, covered, cover, secret, bar, edge_cover, pad_cover):
        widget.installEventFilter(monitor)
    button.clicked.connect(lambda: (clicks.append("submit"), save()))
    covered.clicked.connect(lambda: (clicks.append("covered"), save()))
    other.show()
    main.show()
    cover.setGeometry(covered.rect())
    cover.setAttribute(Qt.WA_TransparentForMouseEvents, False)
    cover.show()
    field.setFocus()
    save()
    app.exec()


def verify(aad, plugin_root):
    assert os.environ.get("WAYLAND_DISPLAY") and not os.environ.get("DISPLAY")

    def call(*args, success=True):
        p = subprocess.run([aad, *args], capture_output=True, text=True, timeout=40)
        if success != (p.returncode == 0):
            raise AssertionError((args, p.returncode, p.stdout, p.stderr))
        return json.loads(p.stdout) if success else p

    def environment(enabled):
        return dict(os.environ, QT_QPA_PLATFORM="wayland", QT_LINUX_ACCESSIBILITY_ALWAYS_ON="1",
                    QT_PLUGIN_PATH=plugin_root, QT_QPA_GENERIC_PLUGINS="aad-wayland-input" if enabled else "",
                    LANG="C", LANGUAGE="C", LC_ALL="C")

    def locate(child):
        def resource_id():
            return next((r["resource_id"] for r in call("apps")["apps"]
                         if r["provider"] == "desktop.linux_atspi"
                         and r["metadata"].get("process_id") == child.pid), None)
        return wait_for(resource_id, "Qt AT-SPI resource")

    def wait_key(resource):
        return wait_for(lambda: next((n for n in call("snapshot", resource)["nodes"]
                                     if "key" in n["capabilities"]), None), "focused Qt key target")

    def control_pointer(resource, name, action):
        with subprocess.Popen([aad, "control", "serve", "--allow-input"], stdin=subprocess.PIPE,
                              stdout=subprocess.PIPE, text=True) as server:
            sid = None
            sequence = 0
            def request(method, **fields):
                nonlocal sid, sequence
                sequence += 1
                message = dict(request_id=str(sequence), session_id=sid, timeout_ms=10000,
                               operation=dict(method=method, **fields))
                server.stdin.write(json.dumps(message) + "\n")
                server.stdin.flush()
                if not select.select([server.stdout], [], [], 12)[0]:
                    raise RuntimeError("control response deadline")
                response = json.loads(server.stdout.readline())
                sid = response["session_id"]
                assert not response.get("error"), response
                return response["data"]
            try:
                request("capabilities")
                request("targets")
                nodes = request("observe", resource_id=resource, view={"mode": "nodes"})["nodes"]
                def target(label):
                    matches = [n["target"] for n in nodes if n.get("name") == label]
                    assert len(matches) == 1, matches
                    return matches[0]
                return request("act", target=target(name), action=action)
            finally:
                server.stdin.close()
                if server.poll() is None:
                    server.terminate()

    def run_app(root, enabled):
        receipt = root / "events.json"
        env = environment(enabled)
        child = subprocess.Popen([sys.executable, __file__, "--app", str(receipt)], env=env)
        try:
            wait_for(receipt.is_file, "native Qt form")
            resource = locate(child)
            def snapshot():
                return json.loads(receipt.read_text())
            if enabled:
                wait_key(resource)
                result = call("do", "key", resource, "--name", "Invoice number", "--keys", "ctrl,a")
                wait_for(lambda: snapshot()["selection"] == "INV-2037", f"Qt key selection ({result})")
                call("do", "key", resource, "--name", "Invoice number", "--keys", "q")
                wait_for(lambda: snapshot()["text"] == "q", "Qt key replacement")
            else:
                call("do", "key", resource, "--name", "Invoice number", "--keys", "ctrl,a", success=False)
            call("do", "click", resource, "--name", "Submit invoice", success=enabled)
            if enabled:
                wait_for(lambda: snapshot()["clicks"] == ["submit"], "submit callback")
                def check_gesture(action, marker, expected, fx, fy, *args):
                    before = len(snapshot()["gestures"])
                    call("do", action, resource, "--name", "Invoice scroll",
                         "--fx", str(fx), "--fy", str(fy), *args)
                    wait_for(lambda: len(snapshot()["gestures"]) > before, action + " receipt")
                    entries = snapshot()["gestures"][before:]
                    assert len(entries) == 1 and entries[0].get(marker) == expected, entries
                    entry = entries[0]
                    assert entry["widget"] == "Invoice scroll", entry
                    for observed, size, fraction in zip(entry["pos"], entry["size"], (fx, fy)):
                        assert abs(observed - max(1, min(size - 2, size * fraction))) <= 1, entry

                for direction, delta in (("down", [0, -240]), ("up", [0, 240]),
                                         ("left", [240, 0]), ("right", [-240, 0])):
                    previous = snapshot()["scroll_value"]
                    check_gesture("scroll", "wheel", delta, 0.78, 0.5, "--direction", direction, "--notches", "2")
                    assert snapshot()["scroll_value"] != previous, snapshot()
                check_gesture("hover", "hover", True, 1, 1, "--ms", "120")
                control_pointer(resource, "Mouse pad", {"type": "click", "button": "right", "count": 2,
                                "modifiers": ["ctrl", "shift"], "fraction": {"fx": 1, "fy": 0}})
                trace = snapshot()["mouse"]
                assert [e["phase"] for e in trace] == ["press", "release", "double", "release"], trace
                assert all(e["button"] == 2 and e["modifiers"] == 0x06000000
                           and e["pos"] == [e["size"][0] - 2, 1] for e in trace), trace
                assert [e["held"] for e in trace] == [2, 0, 2, 0], trace
                call("do", "click", resource, "--name", "Mouse pad", "--button", "middle", "--fx", ".7", "--fy", ".5")
                assert [e["button"] for e in snapshot()["mouse"][4:]] == [4, 4], snapshot()
            before_denied = snapshot()
            call("do", "click", resource, "--name", "Mouse pad", "--button", "right", "--fx", "0",
                 "--fy", "0.5", success=False)
            for action, args in (("scroll", ["--direction", "down"]), ("hover", ["--ms", "1"])):
                # Center is visible. The requested endpoint hits a covering child.
                call("do", action, resource, "--name", "Invoice scroll", "--fx", "0", "--fy", "0.5",
                     *args, success=False)
                call("do", action, resource, "--name", "Other invoice", *args, success=False)
            assert snapshot() == before_denied, snapshot()
            state = snapshot()
            for name in ("Other invoice", "PIN"):
                call("do", "key", resource, "--name", name, "--keys", "q", success=False)
            for name in ("Other invoice", "Covered action", "PIN"):
                call("do", "click", resource, "--name", name, success=False)
            assert snapshot() == state, snapshot()
            assert state["clicks"] == (["submit"] if enabled else []), state
            expected = [["Invoice number", phase] for _ in range(2) for phase in ("key-down", "key-up")]
            expected += [["Submit invoice", phase] for phase in ("mouse-down", "mouse-up")]
            assert state["events"] == (expected if enabled else []), state
            assert state["sibling"] == "unchanged", state
            print(json.dumps({"plugin": enabled, **state}), flush=True)
        finally:
            child.terminate()
            child.wait(timeout=8)

    for enabled in (True, False):
        with tempfile.TemporaryDirectory() as root:
            run_app(Path(root), enabled)

    # Verify the distribution's unmodified KDE application through its own
    # output and exit status, separately from the instrumented event counters.
    dialog = subprocess.Popen(["kdialog", "--inputbox", "Invoice number", "INV-2037"],
                              env=environment(True), stdout=subprocess.PIPE, text=True)
    try:
        resource = locate(dialog)
        field = wait_key(resource)
        for keys in ("ctrl,a", "k"):
            call("do", "key", resource, "--role", field["role"], "--keys", keys)
        call("do", "click", resource, "--name", "OK")
        output, _ = dialog.communicate(timeout=8)
        assert dialog.returncode == 0 and output.strip() == "k", (dialog.returncode, output)
        print(json.dumps({"application": "kdialog", "stdout": output.strip()}))
    finally:
        if dialog.poll() is None:
            dialog.terminate()
            dialog.wait(timeout=8)


if __name__ == "__main__":
    if sys.argv[1] == "--app":
        application(Path(sys.argv[2]))
    else:
        verify(*sys.argv[1:])
