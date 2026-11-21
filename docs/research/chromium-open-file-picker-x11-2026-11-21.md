# Selecting a file in an already-open Chromium picker on X11

`aad do choose-files` completed single- and multiple-file selections in an already-open, browser-owned GTK 3 dialog on Linux X11. The page received the file bytes and the native dialog closed.

```sh
aad do choose-files --cdp-port 9222 --page "$PAGE_ID" --allow-input \
  --allow-upload-dir "$UPLOAD_DIR" --file "$UPLOAD_DIR/one.txt" --file "$UPLOAD_DIR/two.txt"
```

Use the exact page ID from `aad browser-record targets --cdp-port 9222`. The command observes the active native dialog. An optional `--window 0x…` additionally requires a particular XID. An upload grant covers regular files whose canonical paths are under that directory. A local file **page** separately requires `--allow-browser-file-dir`.

### How the selection is bound

1. On the selected page's main frame, the browser provider observes the unique file input with `states.picker_open=true` (Chromium's `:open`). The active element can be a different control. The input can be hidden.
2. The native observation binds the mapped GTK dialog and its transient parent to the CDP browser PID using both X-Resource client ownership and the window PID. The parent must uniquely match the selected CDP window bounds. The active modal must have the GTK file-chooser window role and an accepted Open title. Ambiguous chooser windows are refused.
3. The normal session upload action rechecks the observed input identity and picker state, validates the path grant, and assigns the files. The CLI then revalidates the native window, sends `WM_DELETE_WINDOW` to that XID, and verifies its unmap or destruction. The session holds the browser and desktop input leases throughout. A dismissal failure after successful assignment reports `files_submitted=true` and an applied effect.

The headed regression read both selected files through the page's File API in the requested order. The hidden file input received a trusted change event, its sibling received zero files, and the page saw no cancel event. The wrong tab, wrong XID, and an escaping symlink all left the original picker open. A real Save As dialog remained open after refusal. [Regression test](../../tests/browser/open-native-picker.test.mjs)

The verified configuration used Chrome for Testing 156, a private Xvfb display, xfwm4, and GTK 3. The command accepts the titles `Open File`, `Open Files`, and `打开文件`. It requires a unique match for the native parent in X11 pixel bounds. The tested GTK 4.8.3 dialog lacked the required X11 `WM_WINDOW_ROLE`. Desktop-portal dialogs owned by a separate process also fail the native binding.

## Native-widget experiment

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

These measurements concern direct editing and activation of the GTK widgets. The working CLI path uses the observed DOM input for file assignment and the observed X11 dialog for dismissal.
