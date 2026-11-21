# Window-addressed input for Qt 5 Widgets on Wayland

Start the target application with the `aad-wayland-input` generic plugin. The Linux AT-SPI backend then offers `key_chord` (`aad do key`) on its focused widget and `pointer_click` (`aad do click`) at the center of a widget whose hit can be confirmed. The app must run on native Wayland with Qt 5 Widgets and the Qt AT-SPI bridge. The CLI and app must share the accessibility bus.

## Launch an application

GNU Linux release archives include the plugin in `qt5-plugins/generic`. From the extracted archive directory, run this in PowerShell on the Wayland desktop:

```powershell
$qtPlugins = (Resolve-Path ./qt5-plugins).Path
env "QT_PLUGIN_PATH=$qtPlugins" QT_QPA_GENERIC_PLUGINS=aad-wayland-input QT_QPA_PLATFORM=wayland QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1 kdialog --inputbox 'Invoice number' 'INV-2037'
```

Use the same per-process environment to launch another Qt 5 Widgets app. For single-instance apps, start a fresh process that loads the plugin. An app that ships its own Qt may require a plugin built with its Qt development kit.

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

Use this `$qtPlugins` in the launch command above. Each target process opts in at startup. The GNU local release builder also accepts `-BuildQtWaylandPlugin` to bundle the binary.

## Delivery and limits

The provider resolves the observed widget and its top-level window on the application's unique accessibility-bus owner. The plugin resolves both Qt accessibility IDs on the GUI thread and requires the same active, exposed Wayland window. Keyboard delivery requires that exact focused widget. A click requires both the AT-SPI hit test and the window's own widget hit test. The app receives a Qt press and release, with chord modifiers attached to the key event. A change of receiver, focus or hit after the press yields an unknown effect.

The plugin supports letters, digits, F1–F35, editing/navigation keys and Ctrl, Shift, Alt or Meta chords. Pointer support is a single left click at center. Other buttons, double clicks, off-center positions, drags, hover, scroll, bounded holds, IME typing and seat-wide keyboard or pointer injection are outside this backend. Independent windows can share a coordinate origin on Wayland. Hit verification applies within the addressed window and does not establish compositor-wide visibility.
