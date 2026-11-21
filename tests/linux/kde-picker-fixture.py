"""Observe the real Qt window in the private KWin fixture."""
import sys
import time

import gi
gi.require_version("Atspi", "2.0")
from gi.repository import Atspi


def children(node):
    return [node.get_child_at_index(i) for i in range(node.get_child_count())]


def active_dialog(pid):
    dialogs = []
    for app in children(Atspi.get_desktop(0)):
        if app.get_process_id() != pid or app.get_toolkit_name() != "Qt":
            continue
        for window in children(app):
            window.clear_cache()
            states = window.get_state_set()
            if all(states.contains(s) for s in (Atspi.StateType.ACTIVE, Atspi.StateType.SHOWING)):
                dialogs.append(window)
    if len(dialogs) > 1:
        raise RuntimeError("ambiguous active Qt dialog")
    return dialogs[0] if dialogs else None


if __name__ == "__main__":
    until = time.monotonic() + 10
    while time.monotonic() < until:
        window = active_dialog(int(sys.argv[1]))
        if window is not None and window.get_role() == Atspi.Role.DIALOG:
            print(window.path)
            break
        time.sleep(0.05)
    else:
        sys.exit("KWin did not present an active accessible KDE dialog")
