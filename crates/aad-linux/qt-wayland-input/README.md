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
| `drag`, different widgets | Copy selected text from a drag-enabled, non-password `QLineEdit`, or copy the single selected item of a standard `QListView` or `QListWidget`. The source must be in the active window. The destination can be in another exposed window of the same application. Use the left button with no modifiers or Ctrl. |

```powershell
aad do scroll $resourceId --name 'Invoice scroll' --direction down --notches 2 --fx .78 --fy .5
aad do hover $resourceId --name 'Invoice scroll' --fx 1 --fy 1 --ms 120
aad do click $resourceId --name 'Mouse pad' --button right --count 2 --fx 1 --fy 0
aad do drag $resourceId --name 'Drag selection' --to-name 'Drag selection' --fx 0 --fy .5 --to-fx 1 --to-fy .5
aad do drag $resourceId --name 'Invoice 2037' --to-name 'Remote basket'
```

For a modified click, use the following `action` in a control `act` request with the target handle from that session's `observe` response.

```json
{"type":"click","button":"right","count":2,"fraction":{"fx":1,"fy":0},"modifiers":["ctrl","shift"]}
```

The provider pins both widgets and their respective windows to the observed application's unique AT-SPI bus owner. The GUI thread rechecks identity, exposure and widget hits between phases. An inactive drop target is not eligible for standalone clicks or keys. Hit testing covers each addressed window's own widget hierarchy; no compositor-wide visibility or shared origin is assumed.

For a text drag, start within the selection. The plugin copies up to 65,536 UTF-16 units. For a list drag, address the selected item by its observed identity. The accessible child, the view's hit at the start point and the single selected model index must agree. The model must offer `Qt::CopyAction`. The plugin asks it for the MIME data for that index and copies the bytes of its declared formats. The limit is 16 nonempty formats and 1 MiB total. Custom models can supply their own formats. Custom view subclasses, multiple selections and internal-move lists are outside this path.

Address an accepting widget or empty viewport space in a receiving standard list. The plugin sends drag-enter, drag-move and drop in process, checking the endpoints between phases and requiring copy acceptance. An ignored enter cannot fall through to a parent. Rejection or loss of proof after dispatch begins returns an unknown effect. The clipboard, compositor pointer and source model stay outside the transport. Receivers that require a live `QDrag` source object, cross-process drops, Qt Quick, waypoints and timed holds are unsupported.

To qualify a build from the repository, build `tests/linux/qt_wayland_fixture.pro` with the same qmake as the plugin and run `python3 tests/linux/verify_qt_wayland.py <aad> <fixture-executable> <plugin-root>`. The runner creates a private virtual KWin and D-Bus session. Run the two ABI checks and the existing `verify-wayland-form.ps1` regression serially.
