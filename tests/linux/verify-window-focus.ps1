param(
    [string]$Aad = '',
    [string]$InputHelper = '',
    [string]$BuildDirectory = '',
    [switch]$BuildOnly,
    [switch]$ScopedGeometryOnly
)

# Run alone on an authorized interactive X11 desktop. This fixture independently
# uses a deliberately stale accessibility Focused bit. xdotool and xwininfo
# move and inspect only the fixture's two windows.
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

function Observe-SecondaryScope {
    $frame = Find-Target 'AAD Secondary Window'
    $clock = [Diagnostics.Stopwatch]::StartNew()
    $result = Call-Control @{method='observe';resource_id=$script:resourceId;max_nodes=100;view=@{mode='nodes';scope_target=$frame.target}}
    $elapsed = $clock.Elapsed.TotalMilliseconds
    $nodes = @($result.data.nodes | Where-Object { $_.name -eq 'Live secondary target' })
    if ($result.error -or $result.data.truncated -or $nodes.Count -ne 1 -or
        @($result.data.nodes | Where-Object { $_.name -in @('AAD Primary Window','Stale primary target') }).Count -ne 0) {
        throw 'secondary scope missing its target or exposed the primary window'
    }
    return @{node=$nodes[0];elapsed_ms=$elapsed}
}

function Fixture-Window([string]$title) {
    $windows = @(& xdotool search --onlyvisible --all --pid $fixture.Id --name "^$title$")
    if ($LASTEXITCODE -ne 0 -or $windows.Count -ne 1) { throw "X11 fixture window unavailable: $title" }
    return $windows[0]
}

function Window-Origin([string]$id) {
    $geometry = (& xwininfo -id $id) -join "`n"
    $x = [regex]::Match($geometry, 'Absolute upper-left X:\s*(-?\d+)')
    $y = [regex]::Match($geometry, 'Absolute upper-left Y:\s*(-?\d+)')
    if ($LASTEXITCODE -ne 0 -or -not $x.Success -or -not $y.Success) { throw 'X11 fixture geometry unavailable' }
    return @{x=[int]$x.Groups[1].Value;y=[int]$y.Groups[1].Value}
}

function Move-FixtureWindow([string]$id, [int]$x, [int]$y) {
    $actual = Window-Origin $id
    $moveX = $x; $moveY = $y
    for ($i=0; $i -lt 3; $i++) {
        & xdotool windowmove $id $moveX $moveY
        if ($LASTEXITCODE -ne 0) { throw 'X11 fixture window move failed' }
        for ($poll=0; $poll -lt 20; $poll++) {
            $after = Window-Origin $id
            if ($after.x -eq $x -and $after.y -eq $y) { return }
            if ($after.x -ne $actual.x -or $after.y -ne $actual.y) { break }
            Start-Sleep -Milliseconds 25
        }
        # Reparenting WMs can apply the move to the frame instead of the client.
        $moveX += $x - $after.x; $moveY += $y - $after.y
        $actual = $after
    }
    throw "X11 fixture window did not reach $x,$y"
}

function Test-ScopedGeometry {
    $primary = Fixture-Window 'AAD Primary Window'
    $secondary = Fixture-Window 'AAD Secondary Window'
    & xdotool windowactivate --sync $secondary
    if ($LASTEXITCODE -ne 0) { throw 'secondary window activation failed' }
    $primaryPattern = '^Stale primary target delivered (\d+) released (\d+)$'
    $secondaryPattern = '^Live secondary target delivered (\d+) released (\d+)$'
    $primaryBefore = @(Read-Timing $primaryPattern)
    $secondaryBefore = @(Read-Timing $secondaryPattern)
    $samples = @(for ($i=0; $i -lt 11; $i++) {
        $scoped = Observe-SecondaryScope
        if ($i -ge 2) { $scoped.elapsed_ms }
    })
    # Scope the current revision and keep it across the native window movement.
    $scoped = Observe-SecondaryScope
    $primaryOrigin = Window-Origin $primary
    $secondaryOrigin = Window-Origin $secondary
    Move-FixtureWindow $primary ($primaryOrigin.x + 40) $primaryOrigin.y
    $unchanged = Window-Origin $secondary
    if ($unchanged.x -ne $secondaryOrigin.x -or $unchanged.y -ne $secondaryOrigin.y -or
        (& xdotool getwindowfocus) -ne $secondary) { throw 'the target moved or lost native focus' }
    $action = @{type='key';keys=@('f2')}
    $result = Call-Control @{method='act';target=$scoped.node.target;action=$action}
    $primaryAfter = @(Read-Timing $primaryPattern)
    $secondaryAfter = @(Read-Timing $secondaryPattern)
    $sorted = @($samples | Sort-Object)
    $measurement = @{case='scoped_sibling_moved';effect=$result.effect;code=$result.error.code;
        primary_events=@(($primaryAfter[0]-$primaryBefore[0]),($primaryAfter[1]-$primaryBefore[1]));
        secondary_events=@(($secondaryAfter[0]-$secondaryBefore[0]),($secondaryAfter[1]-$secondaryBefore[1]));
        observe_samples_ms=$samples;observe_median_ms=$sorted[4]}
    $measurement | ConvertTo-Json -Depth 10 | Set-Content -Encoding UTF8 (Join-Path $BuildDirectory 'scoped-geometry.json')
    if ($result.effect -ne 'applied' -or ($measurement.secondary_events -join ',') -ne '1,1' -or
        ($measurement.primary_events -join ',') -ne '0,0') { throw "scoped input after a sibling moved returned $($result.error.code)" }
    $checks.Add($measurement)

    $scoped = Observe-SecondaryScope
    Move-FixtureWindow $secondary ($secondaryOrigin.x + 30) $secondaryOrigin.y
    $result = Call-Control @{method='act';target=$scoped.node.target;action=$action}
    if ($result.effect -ne 'not_applied' -or $result.error.code -ne 'DRIVER.STALE_SNAPSHOT') {
        throw 'moving the scoped target did not invalidate its keyboard anchor'
    }
    $checks.Add(@{case='scoped_target_moved';effect=$result.effect;code=$result.error.code})

    $scoped = Observe-SecondaryScope
    $secondaryOrigin = Window-Origin $secondary
    Move-FixtureWindow $primary $secondaryOrigin.x $secondaryOrigin.y
    & xdotool windowactivate --sync $primary
    if ($LASTEXITCODE -ne 0) { throw 'fixture could not raise the covering window' }
    $result = Call-Control @{method='act';target=$scoped.node.target;action=$action}
    if ($result.effect -ne 'not_applied' -or $result.error.code -ne 'DRIVER.STALE_SNAPSHOT' -or
        ((Read-Timing $primaryPattern) -join ',') -ne ($primaryAfter -join ',') -or
        ((Read-Timing $secondaryPattern) -join ',') -ne ($secondaryAfter -join ',')) {
        throw 'overlapping windows did not reject scoped input with zero extra events'
    }
    $checks.Add(@{case='scoped_target_covered_with_same_geometry';effect=$result.effect;code=$result.error.code})
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

    if ($ScopedGeometryOnly) {
        Test-ScopedGeometry
        @{passed=$true;checks=@($checks)} | ConvertTo-Json -Depth 12 | Set-Content -Encoding UTF8 (Join-Path $BuildDirectory 'summary.json')
        Write-Output 'PASS: scoped X11 geometry preserves the focused target and rejects drift and ambiguity'
        return
    }

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
    Test-ScopedGeometry
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
