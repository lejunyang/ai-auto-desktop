"""Exercise normal CLI actions and independently inspect the target process's UI events."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

from wayland_form import wait_for


def application(receipt):
    from PyQt5.QtCore import QEvent, QObject, Qt, QTimer
    from PyQt5.QtWidgets import QApplication, QLabel, QLineEdit, QPushButton, QVBoxLayout, QWidget

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
    secret.setEchoMode(QLineEdit.Password)
    for widget, name in [(field, "Invoice number"), (sibling, "Other invoice"), (secret, "PIN")]:
        widget.setAccessibleName(name)
    other_box.addWidget(sibling)
    for widget in (field, button, covered, secret):
        box.addWidget(widget)
    events = []
    clicks = []
    def save():
        tmp = receipt.with_suffix(".tmp")
        tmp.write_text(json.dumps(dict(events=events, clicks=clicks, text=field.text(),
                                      selection=field.selectedText(), sibling=sibling.text(),
                                      platform=app.platformName())))
        tmp.replace(receipt)

    class Monitor(QObject):
        def eventFilter(self, obj, event):
            kind = {QEvent.KeyPress: "key-down", QEvent.KeyRelease: "key-up",
                    QEvent.MouseButtonPress: "mouse-down", QEvent.MouseButtonRelease: "mouse-up"}
            if event.type() in kind:
                events.append([obj.accessibleName() or obj.text(), kind[event.type()]])
                QTimer.singleShot(0, save)
            return False

    monitor = Monitor()
    for widget in (sibling, field, button, covered, secret):
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
