param()
$ErrorActionPreference = 'Stop'
Set-Location (Join-Path $PSScriptRoot '../..')
# The service impersonates the portal only on this new private D-Bus. It never
# connects to the desktop portal or displays an authorization dialog.
$env:AAD_PORTAL_TEST_PRIVATE = '1'
try {
    & dbus-run-session -- osdk exec -- cargo test -p aad-linux --lib private_portal_transport -- --ignored --exact wayland_portal::transport::tests::private_portal_transport --nocapture
    if ($LASTEXITCODE -ne 0) { throw 'Private portal transport regression failed' }
} finally {
    Remove-Item Env:AAD_PORTAL_TEST_PRIVATE -ErrorAction SilentlyContinue
}
