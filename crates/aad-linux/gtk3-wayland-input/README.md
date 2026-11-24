# GTK 3 Wayland input module

Build against the target application's GTK 3, ATK bridge, libatspi and libdbus development packages on GNU Linux. Load the module only in the application that should receive input.

```powershell
$module = ./build.ps1 -BuildDirectory ./build
env "GTK_MODULES=$module" GDK_BACKEND=wayland your-gtk3-application
```

The module receives requests on the application's existing AT-SPI bus connection. It uses the ATK bridge's registered object IDs to match the observed widget and its window. The bridge symbols used for this lookup are private exports. Missing exports disable the corresponding capability.

* **Key** targets the focused editable `GtkEntry` in the active window. The accepted keys are lowercase ASCII letters, digits, Left, Right, Backspace and Ctrl+A. The module checks focus, sensitivity, password visibility, input purpose and the window before each GTK event phase.
* **Scroll** sends discrete up, down, left or right events directly to a `GtkRange` or `GtkDrawingArea` exposed as a slider, scroll bar or drawing area. The position is a fraction of the widget's observed allocation. The module walks the active window's mapped widget tree at the resulting window-local point before each notch. Overlapping branches and GTK grabs prevent delivery.

Use `aad snapshot RESOURCE` to check the current capabilities. GTK 4 has a separate accessibility implementation and uses the native AT-SPI actions available on its widgets.
