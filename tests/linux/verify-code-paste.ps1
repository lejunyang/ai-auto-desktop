param(
    [Parameter(Mandatory = $true)][string]$Aad,
    [Parameter(Mandatory = $true)][string]$InputHelper,
    [string]$Code = '/usr/share/code/code',
    [string]$Python = 'python3',
    [string]$Node = 'node',
    [string]$OutputDirectory = ''
)

# Run alone on an authorized X11 desktop. Python 3 and Node with built-in
# WebSocket are needed only for this test; Code uses a fresh isolated profile.
# The Node connection performs read-only CDP verification. Input uses aad.
$ErrorActionPreference = 'Stop'
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-code-paste-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
& $Python (Join-Path $PSScriptRoot 'code_native_paste.py') `
    --aad (Resolve-Path $Aad).Path `
    --helper (Resolve-Path $InputHelper).Path `
    --code (Resolve-Path $Code).Path `
    --node $Node `
    --output (Resolve-Path $OutputDirectory).Path
if ($LASTEXITCODE -ne 0) { throw 'Code paste verification failed; inspect summary and CDP transport evidence' }
