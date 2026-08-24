#!/usr/bin/env python3
"""Small native GTK3 application for the Linux AT-SPI integration test."""

from __future__ import annotations

import gi

gi.require_version("Gtk", "3.0")
from gi.repository import GLib, Gtk


APPLICATION_NAME = "AAD GTK AT-SPI Fixture"
ENTRY_NAME = "Fixture text entry"
BUTTON_NAME = "Invoke fixture button"
STATUS_INITIAL = "Fixture status idle"
STATUS_INVOKED = "Fixture status invoked"


def _set_accessible(widget: Gtk.Widget, name: str, accessible_id: str) -> None:
    accessible = widget.get_accessible()
    accessible.set_name(name)
    # The driver deliberately requires a toolkit-provided stable identity before
    # dispatching writes.  GTK's ATK object exports this as AccessibleId.
    accessible.set_accessible_id(accessible_id)


def main() -> int:
    GLib.set_prgname("aad-gtk-atspi-fixture")
    GLib.set_application_name(APPLICATION_NAME)

    window = Gtk.Window(title=APPLICATION_NAME)
    window.set_default_size(420, 160)
    window.set_border_width(12)
    window.connect("destroy", Gtk.main_quit)
    _set_accessible(window, APPLICATION_NAME, "fixture-window")

    layout = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
    window.add(layout)

    entry = Gtk.Entry()
    entry.set_text("Fixture initial text")
    _set_accessible(entry, ENTRY_NAME, "fixture-entry")
    layout.pack_start(entry, False, False, 0)

    button = Gtk.Button(label="Invoke")
    _set_accessible(button, BUTTON_NAME, "fixture-button")
    layout.pack_start(button, False, False, 0)

    status = Gtk.Label(label=STATUS_INITIAL)
    _set_accessible(status, STATUS_INITIAL, "fixture-status")
    layout.pack_start(status, False, False, 0)

    def on_clicked(_button: Gtk.Button) -> None:
        status.set_text(STATUS_INVOKED)
        status.get_accessible().set_name(STATUS_INVOKED)

    button.connect("clicked", on_clicked)
    window.show_all()
    print("READY", flush=True)
    Gtk.main()
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
