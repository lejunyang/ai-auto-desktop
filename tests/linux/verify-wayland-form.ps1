param(
    [string]$Aad = (Join-Path $PSScriptRoot '../../target/debug/aad'),
    [string]$QtInputPluginRoot,
    [switch]$InsideSession
)
$ErrorActionPreference = 'Stop'
$binary = (Resolve-Path $Aad).Path
if (!$InsideSession) {
    $extra = @()
    if ($QtInputPluginRoot) { $extra = @('-QtInputPluginRoot', (Resolve-Path $QtInputPluginRoot).Path) }
    & dbus-run-session -- (Get-Process -Id $PID).Path -NoProfile -File $PSCommandPath -Aad $binary @extra -InsideSession
    exit $LASTEXITCODE
}

$root = (& mktemp -d).Trim()
if ($LASTEXITCODE -ne 0 -or !$root) { throw 'Could not create a private runtime directory' }
$env:XDG_RUNTIME_DIR = $root
$env:XDG_CONFIG_HOME = Join-Path $root 'config'
$env:XDG_DATA_HOME = Join-Path $root 'data'
$env:XDG_CACHE_HOME = Join-Path $root 'cache'
$env:XDG_SESSION_TYPE = 'wayland'
$env:XDG_CURRENT_DESKTOP = 'KDE'
$env:WAYLAND_DISPLAY = 'aad-form'
$env:GDK_BACKEND = 'wayland'
Remove-Item Env:DISPLAY, Env:AT_SPI_BUS_ADDRESS, Env:NO_AT_BRIDGE -ErrorAction SilentlyContinue
$errors = Join-Path $root 'compositor.log'
$server = Start-Process kwin_wayland -ArgumentList '--virtual','--no-lockscreen','--socket','aad-form' `
    -RedirectStandardOutput (Join-Path $root 'stdout') -RedirectStandardError $errors -PassThru
try {
    $socket = Join-Path $root 'aad-form'
    for ($i = 0; $i -lt 100 -and !(Test-Path $socket); $i++) {
        if ($server.HasExited) { throw (Get-Content $errors -Raw) }
        Start-Sleep -Milliseconds 100
    }
    if (!(Test-Path $socket)) { throw 'KWin did not create the private Wayland socket' }
    if ($QtInputPluginRoot) {
        & /usr/bin/python3 (Join-Path $PSScriptRoot 'wayland_widget_input.py') $binary $QtInputPluginRoot
    } else {
        & /usr/bin/python3 (Join-Path $PSScriptRoot 'wayland_form.py') $binary
    }
    $result = $LASTEXITCODE
} finally {
    if (!$server.HasExited) { $server.Kill(); $server.WaitForExit() }
    Remove-Item -LiteralPath $root -Recurse -Force
}
exit $result
