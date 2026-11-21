# Selecting a file in an already-open Chromium picker on X11

`aad do choose-files` completed file selection in three already-open X11 dialogs on Linux: Chromium's GTK 3 dialog, its GTK 4.8.3 dialog, and the GTK desktop portal. Headed tests independently read both files from the intended page in order and observed the original dialog close.

```sh
aad do choose-files --cdp-port 9222 --page "$PAGE_ID" --allow-input \
  --allow-upload-dir "$UPLOAD_DIR" --file "$UPLOAD_DIR/one.txt" --file "$UPLOAD_DIR/two.txt"
```

Use the exact page ID from `aad browser pages --cdp-port 9222`. The command observes the active native dialog. An optional `--window 0x…` additionally requires a particular XID. An upload grant covers regular files whose canonical paths are under that directory. A local file **page** separately requires `--allow-browser-file-dir`.

### How the selection is bound

1. On the selected page's main frame, the browser provider observes the unique file input with `states.picker_open=true` (Chromium's `:open`). The active element can be a different control. The input can be hidden.
2. The native observation pins the active modal XID, its transient parent, their process identities from X-Resource and their window PID properties. The parent must belong to the CDP browser and uniquely match the selected window bounds. An accepted Open title and the additional backend identity below are both required.
3. The normal session upload action rechecks the observed input identity and picker state, validates the path grant, and assigns the files. The CLI then revalidates the native window, sends `WM_DELETE_WINDOW` to that XID, and verifies its unmap or destruction. The session holds the browser and desktop input leases throughout. A dismissal failure after successful assignment reports `files_submitted=true` and an applied effect.

The headed regression read both selected files through the page's File API in the requested order. The hidden file input received a trusted change event, its sibling received zero files, and the page saw no cancel event. The wrong tab, wrong XID, and an escaping symlink all left the original picker open. A real Save As dialog remained open after refusal. [Regression test](../../tests/browser/open-native-picker.test.mjs)

| Backend | Additional native identity |
| --- | --- |
| Browser-owned GTK 3 | The dialog belongs to the browser PID and publishes the X11 role `GtkFileChooserDialog`. |
| Browser-owned GTK 4 | The dialog lacks the X11 role. Its two `WM_CLASS` strings match the browser PID's registered GTK 4 application name. The sole visible GTK modal has the same title, a `GtkFileChooserWidget` and one Open response button outside the widget. The same AT-SPI objects must survive reobservation. |
| GTK desktop portal | The dialog belongs to the D-Bus owner of `org.freedesktop.impl.portal.desktop.gtk` and has that backend's X11 class and file-chooser role. Frontend and backend must export the same sole live request. Its sender's D-Bus PID must equal Chromium's browser PID. Both service connections and the request path are pinned. [Portal request identity](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.Request.html) |

The verified configuration used Chrome for Testing 156, private Xvfb, xfwm4 and a private session bus. The X11 title allowlist is `Open File`, `Open Files`, and `打开文件`. GTK 4 additionally requires the English Open response name. Its real Save As dialog was refused even after the fixture changed its X11 title to `Open File`. With two real Chromium processes each opening a GTK portal picker, both commands refused and both file inputs stayed empty. The portal route requires a unique request across the two services and a visible X11 transient parent. Other portal implementations and native Wayland require separate verification.

To run the headed regression, set `AAD_TEST_PICKER_X11=1`, `AAD_TEST_CHROME` and `AAD_TEST_CLI` inside the private X11 session and run `node --test tests/browser/open-native-picker.test.mjs`. For a direct GTK dialog, use a private bus without an activatable desktop portal and set `AAD_TEST_PICKER_GTK=3` or `4` (GTK 4 used `GTK_A11Y=atspi` and an English locale). For the portal variant, enable the GTK portal in that bus, query `org.freedesktop.portal.FileChooser.version` before launching Chromium, and set `AAD_TEST_PICKER_PORTAL=1`. The fixture checks the actual X11 role and PID to distinguish the exercised routes.

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
