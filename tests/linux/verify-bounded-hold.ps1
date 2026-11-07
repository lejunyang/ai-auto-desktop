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
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-qt-hold-' + [guid]::NewGuid().ToString('N'))
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
$checks = [Collections.Generic.List[object]]::new()
$script:logPath = Join-Path $BuildDirectory 'native-hold.ndjson'

function Call-Control([hashtable]$operation) {
    $script:sequence++
    $request = @{request_id = "hold-test-$script:sequence"; session_id = $script:sessionId; timeout_ms = 12000; operation = $operation}
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

    $key = Find-Target 'Hold key target'
    # A non-text key measures delivered duration independently of the active
    # IME's composition/filtering of printable letters.
    $result = Call-Control @{method = 'act'; target = $key.target; action = @{type = 'key'; keys = @('f2'); hold_ms = 200}}
    if ($result.effect -ne 'applied') { throw 'bounded key action failed' }
    $timing = @(Read-Timing '^Hold key released (\d+) ms$')
    if ($timing[0] -lt 180) { throw 'delivered key hold was too short' }
    $checks.Add(@{case = 'key'; requested_ms = 200; observed_ms = $timing[0]; released = $true})

    $button = Find-Target 'Hold click target'
    $result = Call-Control @{method = 'act'; target = $button.target; action = @{type = 'click'; hold_ms = 200}}
    if ($result.effect -ne 'applied') { throw 'bounded click action failed' }
    $timing = @(Read-Timing '^Hold click released (\d+) ms$')
    if ($timing[0] -lt 180) { throw 'delivered click hold was too short' }
    $checks.Add(@{case = 'click'; requested_ms = 200; observed_ms = $timing[0]; released = $true})

    $surface = Find-Target 'Hold drag surface'
    $result = Call-Control @{method = 'act'; target = $surface.target; action = @{type = 'drag'; to = $surface.target; fraction = @{fx = 0.2; fy = 0.5}; to_fraction = @{fx = 0.8; fy = 0.5}; hold_ms = 200}}
    if ($result.effect -ne 'applied') { throw 'bounded drag action failed' }
    $timing = @(Read-Timing '^Hold drag released (\d+) ms first move (\d+) ms$')
    if ($timing[1] -lt 180 -or $timing[0] -lt $timing[1]) { throw 'drag moved before requested hold' }
    $checks.Add(@{case = 'drag'; requested_ms = 200; released_ms = $timing[0]; first_move_ms = $timing[1]; released = $true})
    @{passed = $true; helper_sha256 = (Get-FileHash $helperPath).Hash; checks = @($checks)} | ConvertTo-Json -Depth 10 | Set-Content -Encoding UTF8 (Join-Path $BuildDirectory 'summary.json')
    Write-Output 'PASS: real Qt key/button press-release duration and drag dwell independently verified'
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
