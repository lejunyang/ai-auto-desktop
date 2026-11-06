param(
    [string]$Aad = '',
    [string]$InputHelper = '',
    [string]$BuildDirectory = '',
    [switch]$BuildOnly
)

# Run alone on an authorized interactive X11 desktop. This fixture independently
# measures delivered events. Only synthetic status and timing leave its process.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-qt-cancel-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$fixturePath = Join-Path $BuildDirectory 'qt-hold-fixture'
$flags = ((& pkg-config --cflags --libs Qt5Widgets) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'Qt5Widgets development packages unavailable' }
& g++ -std=c++17 -fPIC -O2 -Wall -Wextra -Werror (Join-Path $root 'tests/linux/qt_hold_fixture.cpp') -o $fixturePath @flags
if ($LASTEXITCODE -ne 0) { throw 'Qt hold fixture build failed' }
if ($BuildOnly) { return }
if (-not $Aad -or -not $InputHelper) { throw 'Aad and InputHelper are required for effect verification' }
$helperPath = (Resolve-Path $InputHelper).Path
$script:server = $null
$script:sessionId = $null
$script:resourceId = $null
$fixture = $null
$script:sequence = 0
$script:responses = @{}
$checks = [Collections.Generic.List[object]]::new()
$script:logPath = Join-Path $BuildDirectory 'native-cancel.ndjson'

function Send-Control([hashtable]$operation) {
    $script:sequence++
    $id = "cancel-test-$script:sequence"
    $request = @{request_id = $id; session_id = $script:sessionId; timeout_ms = 12000; operation = $operation}
    $script:server.StandardInput.WriteLine(($request | ConvertTo-Json -Depth 30 -Compress))
    $script:server.StandardInput.Flush()
    if ($operation.method -ne 'targets') { @{request = $request} | ConvertTo-Json -Depth 40 -Compress | Add-Content -Encoding UTF8 $script:logPath }
    return $id
}

function Receive-Control([string]$id) {
    while (-not $script:responses.ContainsKey($id)) {
        $pending = $script:server.StandardOutput.ReadLineAsync()
        if (-not $pending.Wait(15000) -or -not $pending.Result) { throw 'control response deadline exceeded or server stopped' }
        $response = $pending.Result | ConvertFrom-Json
        $script:sessionId = $response.session_id
        $script:responses[$response.request_id] = $response
        # Omit unrelated resource titles from discovery responses.
        if (-not $response.data.resources) { @{response = $response} | ConvertTo-Json -Depth 40 -Compress | Add-Content -Encoding UTF8 $script:logPath }
    }
    $response = $script:responses[$id]
    $script:responses.Remove($id)
    return $response
}

function Call-Control([hashtable]$operation) { return Receive-Control (Send-Control $operation) }

function Fixture-Event([string]$prefix) {
    $until = [DateTime]::UtcNow.AddSeconds(5)
    while ([DateTime]::UtcNow -lt $until) {
        $line = $fixture.StandardOutput.ReadLineAsync()
        if (-not $line.Wait(5000) -or -not $line.Result) { throw 'independent fixture event missing' }
        if ($line.Result.StartsWith($prefix)) { return $line.Result }
    }
    throw 'expected fixture event missing'
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
    $fixtureInfo.Arguments = '--report-events'
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

    foreach ($kind in @('key', 'click', 'drag')) {
        $name = if ($kind -eq 'drag') { 'Hold drag surface' } else { "Hold $kind target" }
        $node = Find-Target $name
        $action = @{type = $kind; hold_ms = 5000}
        if ($kind -eq 'key') { $action.keys = @('f2') }
        if ($kind -eq 'drag') {
            $action.to = $node.target
            $action.fraction = @{fx = 0.2; fy = 0.5}
            $action.to_fraction = @{fx = 0.8; fy = 0.5}
        }
        $running = Send-Control @{method = 'act'; target = $node.target; action = $action}
        Fixture-Event "$kind down" | Out-Null
        # The app has independently seen the down event before cancellation.
        $clock = [Diagnostics.Stopwatch]::StartNew()
        $cancel = Call-Control @{method = 'cancel'; request_id = $running}
        if (-not $cancel.data.cancellation_requested) { throw 'control cancellation was not accepted' }
        $released = Fixture-Event "$kind up "
        $releaseLatency = $clock.ElapsedMilliseconds
        $result = Receive-Control $running
        if (-not $result.error -or $result.error.code -notmatch 'CANCELLED$' -or $result.effect -ne 'unknown') { throw 'cancelled input result did not preserve uncertain effects' }
        $duration = [long]($released -replace '^.* up ', '')
        if ($releaseLatency -gt 1500 -or $duration -ge 4000) { throw 'input remained held until the original hold duration' }
        $checks.Add(@{case = $kind; requested_ms = 5000; actual_hold_ms = $duration; cancel_to_release_ms = $releaseLatency; code = $result.error.code; released = $true})
        # A cancelled invocation must not poison a subsequent observation/token.
        Observe-Fixture | Out-Null
    }
    $node = Find-Target 'Hold key target'
    $result = Call-Control @{method = 'act'; target = $node.target; action = @{type = 'key'; keys = @('f2'); hold_ms = 180}}
    if ($result.effect -ne 'applied') { throw 'next invocation inherited cancelled state' }
    Fixture-Event 'key down' | Out-Null
    Fixture-Event 'key up ' | Out-Null
    $checks.Add(@{case = 'next_invocation'; effect = 'applied'})
    @{passed = $true; helper_sha256 = (Get-FileHash $helperPath).Hash; checks = @($checks)} | ConvertTo-Json -Depth 10 | Set-Content -Encoding UTF8 (Join-Path $BuildDirectory 'summary.json')
    Write-Output 'PASS: control_cancel releases delivered key/button/drag before bounded hold expiry; next invocation succeeds'

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
