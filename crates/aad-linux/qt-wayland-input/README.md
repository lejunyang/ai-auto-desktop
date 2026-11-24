# Window-addressed input for Qt 5 and Qt 6 Widgets on Wayland

Start the target Qt Widgets application with the `aad-wayland-input` generic plugin. The app must run on native Wayland with the Qt AT-SPI bridge. The CLI and app must share the accessibility bus.

## Launch an application

GNU Linux release archives include separate `qt5-plugins/generic` and `qt6-plugins/generic` builds. Select the directory matching the application's Qt major version. For a Qt 5 KDialog, from the extracted archive directory:

```powershell
$qtPlugins = (Resolve-Path ./qt5-plugins).Path
env "QT_PLUGIN_PATH=$qtPlugins" QT_QPA_GENERIC_PLUGINS=aad-wayland-input QT_QPA_PLATFORM=wayland QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 kdialog --inputbox 'Invoice number' 'INV-2037'
```

Use the same per-process environment to launch another Widgets app. For single-instance apps, start a fresh process that loads the plugin. An app that ships its own Qt may require a plugin built with its Qt development kit.

From a second PowerShell terminal in the same desktop session, select KDialog's resource ID from `aad apps`. Use the focused field's observed role and the button name from `aad snapshot`. For the English input box:

```powershell
aad apps
$resourceId = Read-Host 'KDialog resource_id'
$nodes = (aad snapshot $resourceId | ConvertFrom-Json).nodes
$field = $nodes | Where-Object { $_.capabilities -contains 'key' } | Select-Object -First 1
aad do key $resourceId --role $field.role --keys 'ctrl,a'
aad do key $resourceId --role $field.role --keys k
aad do click $resourceId --name OK
```

The first terminal prints `k` when KDialog accepts the change.

## Build for the target's Qt installation

Every Linux archive includes this source kit in `qt-wayland-input`. It also lives under `crates/aad-linux` in the repository. Install Qt 5 development tools and headers, GNU Make and a C++17 compiler. On Debian the build packages are `qtbase5-dev`, `qt5-qmake` and `build-essential`. The target also needs its Qt Wayland platform plugin (Debian package `qtwayland5`).

From this source kit directory, build into a user-owned directory:

```powershell
$qtPlugins = Join-Path $HOME '.local/share/aad/qt5-plugins'
pwsh -NoProfile -File ./build.ps1 -BuildDirectory $qtPlugins
```

For Qt 6, install `qt6-base-dev`, `qmake6` and `qt6-wayland`; build with `-QtMajor 6` into a separate `qt6-plugins` directory. `-Qmake` can select a matching SDK executable. Use the matching `$qtPlugins` in the launch command above. Each target process opts in at startup. The GNU local release builder accepts `-BuildQtWaylandPlugin` to bundle both binaries.

## Actions on an observed widget

Use `aad snapshot $resourceId` to find widget names and their current capabilities. Pointer capabilities require a clear center when observed. Each requested endpoint must also pass a fresh AT-SPI hit test and the target window's exact QWidget hit test. Fractions run from 0 to 1 along each widget axis. Omitting the fraction uses the center. The following examples use widget names from the qualification form.

| Action | Qt delivery and limits |
|---|---|
| `key` | Press and release on the focused widget. Supports ASCII letters and digits, F1–F35, editing/navigation keys and Ctrl, Shift, Alt or Meta chords. |
| `click` | Left, middle or right button, single or double click, at an optional fraction. Clicks and drags accept Ctrl, Shift, Alt and Meta through the control API. |
| `scroll` | One wheel event at the requested fraction, with 1–30 detents along one of four directions. The widget decides how far to scroll. |
| `hover` | One widget-local hover move with optional dwell. During dwell the backend rechecks the hit. The system cursor stays in place. |
| `drag`, same widget | A press, one held-button move and a release. The plugin checks the straight path for covering children before press and again before move. The longer axis may span up to 8192 logical pixels. |
| `drag`, different widgets | Copy selected plain text from a drag-enabled, non-password `QLineEdit` in the active window into an exact `acceptDrops()` widget. Both must belong to the same observed application; the destination may be in another exposed, non-minimized window. Use the left button with no modifiers or Ctrl. |

```powershell
aad do scroll $resourceId --name 'Invoice scroll' --direction down --notches 2 --fx .78 --fy .5
aad do hover $resourceId --name 'Invoice scroll' --fx 1 --fy 1 --ms 120
aad do click $resourceId --name 'Mouse pad' --button right --count 2 --fx 1 --fy 0
aad do drag $resourceId --name 'Drag selection' --to-name 'Drag selection' --fx 0 --fy .5 --to-fx 1 --to-fy .5
```

For a modified click, use the following `action` in a control `act` request with the target handle from that session's `observe` response.

```json
{"type":"click","button":"right","count":2,"fraction":{"fx":1,"fy":0},"modifiers":["ctrl","shift"]}
```

The provider pins both widgets and their respective windows to the observed application's unique AT-SPI bus owner. The GUI thread rechecks identity, exposure and widget hits between phases. An inactive drop target is not eligible for standalone clicks or keys. Hit testing covers each addressed window's own widget hierarchy; no compositor-wide visibility or shared origin is assumed.

For a cross-widget drag, start within the selection. The plugin copies the actual selection (up to 65,536 UTF-16 units) into in-process MIME data; it never reads the clipboard or sends the payload over D-Bus. It dispatches drag-enter, drag-move and drop to the destination and requires acceptance of the copy. An ignored enter cannot fall through to a parent drop handler. Loss of proof or rejection after delivery starts is an unknown effect, not a successful drop. This is a directed copy, not a seat drag: no source deletion, compositor pointer motion, or `QDrag` source object. Handlers that require such an object, arbitrary MIME, cross-process drops, Qt Quick, waypoints and timed holds are not supported.

To qualify a build from the repository, build `tests/linux/qt_wayland_fixture.pro` with the same qmake as the plugin and run `python3 tests/linux/verify_qt_wayland.py <aad> <fixture-executable> <plugin-root>`. The runner creates a private virtual KWin and D-Bus session. Run the two ABI checks and the existing `verify-wayland-form.ps1` regression serially.
