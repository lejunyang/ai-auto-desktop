# Selecting a file in an already-open Chromium picker on X11

The current Linux provider cannot produce a writable filename entry and Open button on either of the two Chromium picker configurations tested. Window ownership could be established in both. This limits what an `aad do choose-files` command could implement using the existing provider.

## Headed reproduction

Google Chrome for Testing 156.0.8078.4 ran on a private Xvfb display with xfwm4, a private session bus, and a temporary browser profile. The fixture page contained a file input with ID `f` and a sibling text input with ID `s` and value `sibling`. A page CDP session called `f.click()` with `userGesture: true`. File chooser interception was off. The native control tests started after X11 mapped the dialog.

1. Start the window manager and Chromium inside the private display and bus. Use `XDG_SESSION_TYPE=x11`, `XDG_CURRENT_DESKTOP=XFCE`, `GDK_BACKEND=x11`, `GTK_USE_PORTAL=0`, and Chromium's `--ozone-platform=x11 --remote-debugging-port=0 --user-data-dir=<fresh-directory>`. Select `--gtk-version=3` or `--gtk-version=4`. The GTK 4 run used `GTK_A11Y=atspi`.
2. Click the file input once and wait for a viewable dialog. The first dialog took about 25 seconds while the private desktop portal was starting. Allow 60 seconds and account for the browser's locale when looking up its XID.
3. Compare `xprop -id <xid> _NET_WM_PID WM_TRANSIENT_FOR WM_WINDOW_ROLE` with the browser entry from the loopback browser CDP method `SystemInfo.getProcessInfo`. Inspect `aad apps` and `aad snapshot <resource>` for the matching native application. To test the best case for filename input, expose the location field with Ctrl+L as a fixture user on the isolated display.
4. After the native control attempt, independently evaluate `[f.files.length, s.value]` on the page. Success would require reading back the selected file's expected name and contents.

## Measurements

| Observation | GTK 3 | GTK 4.8.3 |
| --- | --- | --- |
| X11 ownership | `GtkFileChooserDialog`, PID 2211991, transient for the Chrome window | PID 2331180, transient for the Chrome window |
| Browser PID from CDP | 2211991 | 2331180 |
| Native observation | 292 nodes, complete. Exact `file_chooser` search returned zero matches. | A separate GTK resource with the browser PID contained a top-level `dialog` and a child named `GtkFileChooserWidget`. |
| Entry and Open action | `aad do invoke` for the Open button returned `DRIVER.NOT_FOUND`. | The focused, editable `GtkFileChooserEntry` and the `_Open Open` button both had empty capabilities. Exact `aad do fill` and `aad do invoke` each returned `CONTROL.ACTION_UNSUPPORTED`, effect `not_applied`. |
| Independent page read | `[0, "sibling"]` | `[0, "sibling"]` |

On GTK 3 a diagnostic AT-SPI listener saw the location field receive focus. Its actual parent chain included the location layer, the embedded chooser, the top-level `file chooser` named `Open File`, and a GTK application named `chrome`. That application had a **different object path** from the Chromium application root returned by the accessibility registry on the same bus connection. Walking the registered root and reading its AT-SPI cache both omitted the picker. An event object path alone cannot be used as a snapshot target. Chromium deliberately defers GTK bridge initialization. [Chromium GTK initialization](https://chromium.googlesource.com/chromium/src/+/master/ui/gtk/gtk_ui.cc)

On GTK 4 the entry and Open button reported `visible=true`, `sensitive=true`, `enabled=false`, and `showing=false`. The button published a native click action. The Linux provider requires enabled and sensitive to offer semantic input. The entry and button therefore remain observable through the CLI without a dispatchable action.

## Platform work needed

GTK 3 would need native discovery of the detached chooser **after** it opens, plus a way to retain and revalidate its controls in the same X11 window. GTK 4 would need toolkit-specific proof of control state and window identity before the provider could offer actions. A successful implementation must independently verify the granted file on the page and reject a different active window, ambiguous controls, and Save As dialogs.

[待验证] Multi-file behavior requires a writable, revalidated native picker and a page with `<input type="file" multiple>` on a headed X11 browser.
