# GTK 3 Wayland input module

Build against the target application's GTK 3, ATK bridge, libatspi and libdbus development packages on GNU Linux. Load the module only in the application that should receive input.

```powershell
$module = ./build.ps1 -BuildDirectory ./build
env "GTK_MODULES=$module" GDK_BACKEND=wayland your-gtk3-application
```

The module receives requests on the application's existing AT-SPI bus connection. It uses the ATK bridge's registered object IDs to match the observed widget and its window. The bridge symbols used for this lookup are private exports. Missing exports disable the corresponding capability.

* **Key** targets the focused editable `GtkEntry` in the active window. The accepted keys are lowercase ASCII letters, digits, Left, Right, Backspace and Ctrl+A. The module checks focus, sensitivity, password visibility, input purpose and the window before each GTK event phase.
* **Scroll** sends discrete up, down, left or right events directly to a `GtkRange` or `GtkDrawingArea` exposed as a slider, scroll bar or drawing area. The position is a fraction of the widget's observed allocation. The module walks the active window's mapped widget tree at the resulting window-local point before each notch. Overlapping branches and GTK grabs prevent delivery.
* **Click** sends one unmodified left-button press and release to a `GtkButton` or a `GtkDrawingArea` listening for both events. Use `aad do click RESOURCE --name NAME` for the center, or add `--fx .75 --fy .5` for a position inside the widget. The module advertises click when the center has an exclusive GTK widget and GDK event-window hit. It checks the requested point before pressing and again on the next main-loop turn before releasing. A covered or closed target after the press yields an unknown effect. A plain button label can share the button's hit.

If the release loses its hit, the module cancels the press on its original live receiver. A standard button clears its pressed state, and the GTK gesture controller that still owns that press is reset. Custom drawing areas can handle `grab-broken-event` to clear their own application state.

Use `aad snapshot RESOURCE` to check the current capabilities. GTK 4 has a separate accessibility implementation and uses the native AT-SPI actions available on its widgets.
