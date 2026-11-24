param([string]$Aad = (Join-Path $PSScriptRoot '../../target/debug/aad'))
$ErrorActionPreference = 'Stop'
$binary = (Resolve-Path $Aad).Path
$root = (& mktemp -d).Trim()
if ($LASTEXITCODE -or !$root) { throw 'Could not create a private runtime directory' }
try {
    $module = & (Join-Path $PSScriptRoot '../../crates/aad-linux/gtk3-wayland-input/build.ps1') $root
    # Set the environment before dbus-daemon starts so activated AT-SPI
    # services also use the private runtime and display.
    $env:XDG_RUNTIME_DIR = $root
    $env:XDG_CONFIG_HOME = Join-Path $root 'config'
    $env:XDG_CACHE_HOME = Join-Path $root 'cache'
    $env:XDG_DATA_HOME = Join-Path $root 'data'
    $env:XDG_SESSION_TYPE = 'wayland'
    $env:WAYLAND_DISPLAY = 'aad-gtk'
    $env:GDK_BACKEND = 'wayland'
    $env:GTK_A11Y = 'atspi'
    $env:PYTHONDONTWRITEBYTECODE = '1'
    Remove-Item Env:DISPLAY, Env:AT_SPI_BUS_ADDRESS, Env:NO_AT_BRIDGE, Env:GTK_MODULES -ErrorAction SilentlyContinue
    & dbus-run-session -- /usr/bin/python3 (Join-Path $PSScriptRoot 'wayland_gtk_input.py') $binary $module
    $result = $LASTEXITCODE
} finally {
    Remove-Item -LiteralPath $root -Recurse -Force
}
exit $result
