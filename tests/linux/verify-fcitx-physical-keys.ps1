param(
    [string]$Aad = '',
    [string]$InputHelper = '',
    [string]$BuildDirectory = '',
    [string]$ComposingMethod = 'pinyin',
    [string]$Keystrokes = 'nihao',
    [string]$ExpectedText = '你好',
    [switch]$BuildOnly
)

# Run alone on an authorized X11 desktop with Fcitx5/Qt5. The test selects its
# own focused context's IME explicitly, then restores it. Production typing
# never changes the method. PS 5.1 syntax; all input goes through aad.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-ime-keys-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$binary = Join-Path $BuildDirectory 'qt-ime-fixture'
$flags = ((& pkg-config --cflags --libs Qt5Widgets) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'Qt5Widgets development packages unavailable' }
& g++ -std=c++17 -fPIC -O2 -Wall -Wextra -Werror (Join-Path $root 'tests/linux/qt_ime_fixture.cpp') -o $binary @flags
if ($LASTEXITCODE -ne 0) { throw 'Qt IME fixture build failed' }
if ($BuildOnly) { return }
if (-not $Aad -or -not $InputHelper) { throw 'Aad and InputHelper required' }
$helperPath = (Resolve-Path $InputHelper).Path
$script:server = $null
$script:sessionId = $null
$script:resourceId = $null
$script:sequence = 0
$fixture = $null
$originalMethod = $null
$passed = $false
$checks = [Collections.Generic.List[object]]::new()

function Call-Control([hashtable]$operation) {
    $script:sequence++
    $request = @{request_id = "ime-keys-$script:sequence"; session_id = $script:sessionId; timeout_ms = 12000; operation = $operation}
    $script:server.StandardInput.WriteLine(($request | ConvertTo-Json -Depth 30 -Compress))
    $script:server.StandardInput.Flush()
    $pending = $script:server.StandardOutput.ReadLineAsync()
    if (-not $pending.Wait(15000) -or -not $pending.Result) { throw 'control response missing or timed out' }
    $response = $pending.Result | ConvertFrom-Json
    $script:sessionId = $response.session_id
    if ($operation.method -eq 'observe' -or $operation.method -eq 'act') {
        @{request = $request; response = $response} | ConvertTo-Json -Depth 40 -Compress | Add-Content -Encoding UTF8 (Join-Path $BuildDirectory 'native-ime.ndjson')
    }
    return $response
}
function Find-Entry {
    $response = Call-Control @{method = 'observe'; resource_id = $script:resourceId; max_nodes = 100; view = @{mode = 'nodes'}}
    if ($response.error -or $response.data.truncated) { throw 'IME fixture observation incomplete' }
    $entries = @($response.data.nodes | Where-Object { $_.name -eq 'IME test entry' -and $_.states.editable })
    if ($entries.Count -ne 1) { throw 'IME entry missing or ambiguous' }
    return $entries[0]
}
function Act-Entry([hashtable]$action) {
    $entry = Find-Entry
    return Call-Control @{method = 'act'; target = $entry.target; action = $action}
}
function Invoke-InputMethod([string]$Method, [string[]]$Arguments = @()) {
    $reply = & gdbus call --session --dest org.fcitx.Fcitx5 --object-path /controller --method "org.fcitx.Fcitx.Controller1.$Method" @Arguments
    if ($LASTEXITCODE -ne 0) { throw "Fcitx5 $Method failed" }
    return ($reply -join "`n")
}
function Set-TestMethod([string]$method) {
    Invoke-InputMethod 'SetCurrentIM' @($method) | Out-Null
    for ($i = 0; $i -lt 30; $i++) {
        if ((Invoke-InputMethod 'CurrentInputMethod') -ceq "('$method',)") { return }
        Start-Sleep -Milliseconds 100
    }
    throw 'test input method did not become active'
}
function Read-State { return (Get-Content (Join-Path $BuildDirectory 'fixture-state.json') -Raw | ConvertFrom-Json) }

try {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $binary
    $info.UseShellExecute = $false
    $info.RedirectStandardOutput = $true
    $info.EnvironmentVariables['QT_ACCESSIBILITY'] = '1'
    $info.EnvironmentVariables['QT_LINUX_ACCESSIBILITY_ALWAYS_ON'] = '1'
    $info.EnvironmentVariables['QT_IM_MODULE'] = 'fcitx'
    $info.EnvironmentVariables['AAD_IME_TEST_OUTPUT'] = $BuildDirectory
    $fixture = [Diagnostics.Process]::Start($info)
    $ready = $fixture.StandardOutput.ReadLineAsync()
    if (-not $ready.Wait(10000) -or $ready.Result -ne 'READY') { throw 'Qt IME fixture failed to start' }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = (Resolve-Path $Aad).Path
    $info.Arguments = 'control serve --allow-input'
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.EnvironmentVariables['AAD_LINUX_XTEST_HELPER'] = $helperPath
    $script:server = [Diagnostics.Process]::Start($info)
    Call-Control @{method = 'capabilities'} | Out-Null
    $found = @()
    for ($i = 0; $i -lt 30; $i++) {
        $response = Call-Control @{method = 'targets'}
        $found = @($response.data.resources | Where-Object { $_.metadata.process_id -eq $fixture.Id })
        if ($found.Count -eq 1) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($found.Count -ne 1) { throw 'Qt IME resource missing' }
    $script:resourceId = $found[0].resource_id
    if ((Act-Entry @{type = 'focus'}).effect -ne 'applied') { throw 'fixture focus failed' }
    $reply = Invoke-InputMethod 'CurrentInputMethod'
    if ($reply -notmatch "^\('([A-Za-z0-9_-]+)',\)$") { throw 'unrecognized original input method' }
    $originalMethod = $Matches[1]
    Set-TestMethod $ComposingMethod
    $denied = Act-Entry @{type = 'type_text'; text = $Keystrokes}
    if (($denied | ConvertTo-Json -Depth 40) -notmatch 'DRIVER.INPUT_METHOD_ACTIVE' -or (Read-State).text -cne '') { throw 'default Unicode mode did not preserve its composition guard' }
    $checks.Add(@{case = 'default_unicode_refused'; unchanged = $true})
    if ((Act-Entry @{type = 'type_text'; mode = 'keys'; text = $Keystrokes}).effect -ne 'applied') { throw 'physical IME typing failed' }
    for ($i = 0; $i -lt 30; $i++) {
        $state = Read-State
        if ($state.preedit_seen -and $state.preedit -and (Test-Path (Join-Path $BuildDirectory 'candidate.png'))) { break }
        Start-Sleep -Milliseconds 100
    }
    if (-not $state.preedit_seen -or -not $state.preedit -or $state.text -cne '') { throw 'Qt did not observe active preedit before selection' }
    $checks.Add(@{case = 'preedit'; state = $state; candidate_image = 'candidate.png'})
    if ((Act-Entry @{type = 'key'; keys = @('space')}).effect -ne 'applied') { throw 'candidate selection failed' }
    for ($i = 0; $i -lt 30; $i++) {
        $state = Read-State
        if ($state.text -ceq $ExpectedText -and $state.commits -gt 0) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($state.text -cne $ExpectedText -or $state.last_commit -cne $ExpectedText -or $state.preedit) { throw 'independent Qt commit differs from selected candidate' }
    if ((Find-Entry).value -cne $ExpectedText) { throw 'fresh AT-SPI text differs from Qt commit' }
    $checks.Add(@{case = 'commit'; state = $state; fresh_atspi_matches = $true})
    $passed = $true
    Write-Output 'PASS: default Unicode refused, physical keys composed, space committed Chinese, Qt events and fresh AT-SPI agree'
} finally {
    try {
        if ($originalMethod -and $fixture -and -not $fixture.HasExited -and $script:server -and -not $script:server.HasExited) {
            # Restore only after re-establishing the test's own observed focus.
            if ((Act-Entry @{type = 'focus'}).effect -ne 'applied') { throw 'cannot safely restore test input method without fixture focus' }
            Set-TestMethod $originalMethod
        }
    } finally {
        try {
            if ($script:server -and -not $script:server.HasExited) {
                try { Call-Control @{method = 'close'} | Out-Null } finally {
                    $script:server.StandardInput.Close()
                    if (-not $script:server.WaitForExit(15000)) { $script:server.Kill(); $script:server.WaitForExit() }
                }
            }
        } finally {
            if ($fixture -and -not $fixture.HasExited) { $fixture.Kill(); $fixture.WaitForExit() }
            @{passed = $passed; helper_sha256 = (Get-FileHash $helperPath).Hash; input_method = $ComposingMethod; checks = @($checks)} | ConvertTo-Json -Depth 20 | Set-Content -Encoding UTF8 (Join-Path $BuildDirectory 'summary.json')
        }
    }
}
