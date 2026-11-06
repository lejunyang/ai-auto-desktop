param(
    [string]$Aad = '',
    [string]$InputHelper = '',
    [string]$BuildDirectory = '',
    [switch]$BuildOnly
)

# Run alone on an authorized interactive X11 desktop. This fixture independently
# uses a deliberately stale accessibility Focused bit. Only counters leave its process.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-qt-window-focus-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$fixturePath = Join-Path $BuildDirectory 'qt-window-focus-fixture'
$flags = ((& pkg-config --cflags --libs Qt5Widgets) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'Qt5Widgets development packages unavailable' }
& g++ -std=c++17 -fPIC -O2 -Wall -Wextra -Werror (Join-Path $root 'tests/linux/qt_window_focus_fixture.cpp') -o $fixturePath @flags
if ($LASTEXITCODE -ne 0) { throw 'Qt window focus fixture build failed' }
if ($BuildOnly) { return }
if (-not $Aad -or -not $InputHelper) { throw 'Aad and InputHelper are required for effect verification' }
$helperPath = (Resolve-Path $InputHelper).Path
$script:server = $null
$script:sessionId = $null
$script:resourceId = $null
$fixture = $null
$script:sequence = 0
$checks = [Collections.Generic.List[object]]::new()
$script:logPath = Join-Path $BuildDirectory 'native-window-focus.ndjson'

function Call-Control([hashtable]$operation) {
    $script:sequence++
    $request = @{request_id = "window-focus-test-$script:sequence"; session_id = $script:sessionId; timeout_ms = 12000; operation = $operation}
    $script:server.StandardInput.WriteLine(($request | ConvertTo-Json -Depth 30 -Compress))
    $script:server.StandardInput.Flush()
    $pending = $script:server.StandardOutput.ReadLineAsync()
    if (-not $pending.Wait(15000)) { throw 'control response deadline exceeded' }
    if (-not $pending.Result) { throw 'control server stopped' }
    $response = $pending.Result | ConvertFrom-Json
    $script:sessionId = $response.session_id
    # Targets can list unrelated windows; only log actions and our own fixture.
    if ($operation.method -eq 'act' -or $operation.method -eq 'observe') {
        @{request = $request; response = $response} | ConvertTo-Json -Depth 40 -Compress | Add-Content -Encoding UTF8 $script:logPath
    }
    return $response
}

function Observe-Fixture {
    $result = Call-Control @{method = 'observe'; resource_id = $script:resourceId; max_nodes = 100; view = @{mode = 'nodes'}}
    if ($result.error -or $result.data.truncated) { throw 'fixture observation failed or truncated' }
    return $result.data
}

function Find-Target([string]$name) {
    $nodes = @((Observe-Fixture).nodes | Where-Object { $_.name -eq $name })
    if ($nodes.Count -ne 1) { throw "fixture target missing or ambiguous: $name" }
    return $nodes[0]
}

function Read-Timing([string]$pattern) {
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 100
        foreach ($node in (Observe-Fixture).nodes) {
            $match = [regex]::Match($node.name, $pattern)
            if ($match.Success) {
                return @($match.Groups | Select-Object -Skip 1 | ForEach-Object { [long]$_.Value })
            }
        }
    }
    throw 'independent fixture timing status missing'
}

try {
    $fixtureInfo = [Diagnostics.ProcessStartInfo]::new()
    $fixtureInfo.FileName = $fixturePath
    $fixtureInfo.UseShellExecute = $false
    $fixtureInfo.RedirectStandardOutput = $true
    $fixture = [Diagnostics.Process]::Start($fixtureInfo)
    $ready = $fixture.StandardOutput.ReadLineAsync()
    if (-not $ready.Wait(10000) -or $ready.Result -ne 'READY') { throw 'fixture failed to start' }
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
    for ($i = 0; $i -lt 20; $i++) {
        $targets = Call-Control @{method = 'targets'}
        $found = @($targets.data.resources | Where-Object { $_.metadata.process_id -eq $fixture.Id })
        if ($found.Count -eq 1) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($found.Count -ne 1) { throw 'fixture resource not found' }
    $script:resourceId = $found[0].resource_id

    $stale = Find-Target 'Stale primary target'
    if (-not $stale.states.focused -or $stale.capabilities -notcontains 'key') { throw 'fixture did not expose its deliberate stale Focused bit' }
    $result = Call-Control @{method = 'act'; target = $stale.target; action = @{type = 'key'; keys = @('f2')}}
    if (-not $result.error -or $result.effect -eq 'applied') { throw 'same-process sibling focus was accepted' }
    $timing = @(Read-Timing '^Stale primary target delivered (\d+) released (\d+)$')
    if ($timing[0] -ne 0 -or $timing[1] -ne 0) { throw 'stale primary received denied input' }
    $timing = @(Read-Timing '^Live secondary target delivered (\d+) released (\d+)$')
    if ($timing[0] -ne 0 -or $timing[1] -ne 0) { throw 'sibling received denied input' }
    $checks.Add(@{case = 'same_pid_sibling_denied'; code = $result.error.code; primary_events = 0; sibling_events = 0})

    $live = Find-Target 'Live secondary target'
    $result = Call-Control @{method = 'act'; target = $live.target; action = @{type = 'key'; keys = @('f2'); hold_ms = 180}}
    if ($result.effect -ne 'applied') { throw 'focused secondary input failed' }
    $timing = @(Read-Timing '^Live secondary target delivered (\d+) released (\d+)$')
    if ($timing[0] -ne 1 -or $timing[1] -ne 1) { throw 'secondary actual press-release mismatch' }
    $checks.Add(@{case = 'focused_secondary'; delivered = $timing[0]; released = $timing[1]})

    $frame = Find-Target 'AAD Primary Window'
    $result = Call-Control @{method = 'act'; target = $frame.target; action = @{type = 'activate'}}
    if ($result.effect -ne 'applied') { throw 'primary activation failed' }
    $stale = Find-Target 'Stale primary target'
    $result = Call-Control @{method = 'act'; target = $stale.target; action = @{type = 'key'; keys = @('f2')}}
    if ($result.effect -ne 'applied') { throw 'bound primary input failed after activation' }
    $timing = @(Read-Timing '^Stale primary target delivered (\d+) released (\d+)$')
    if ($timing[0] -ne 1 -or $timing[1] -ne 1) { throw 'primary actual press-release mismatch' }
    $checks.Add(@{case = 'activated_primary'; delivered = $timing[0]; released = $timing[1]})
    @{passed = $true; helper_sha256 = (Get-FileHash $helperPath).Hash; checks = @($checks)} | ConvertTo-Json -Depth 10 | Set-Content -Encoding UTF8 (Join-Path $BuildDirectory 'summary.json')
    Write-Output 'PASS: stale AX focus cannot target a same-PID sibling; each correct window receives and releases its own input'

} finally {
    try {
        if ($script:server -and -not $script:server.HasExited) {
            try { Call-Control @{method = 'close'} | Out-Null } finally {
                $script:server.StandardInput.Close()
                if (-not $script:server.WaitForExit(15000)) { $script:server.Kill(); $script:server.WaitForExit() }
            }
        }
    } finally {
        if ($fixture -and -not $fixture.HasExited) {
            if (-not $fixture.CloseMainWindow() -or -not $fixture.WaitForExit(2000)) { $fixture.Kill(); $fixture.WaitForExit() }
        }
    }
}
