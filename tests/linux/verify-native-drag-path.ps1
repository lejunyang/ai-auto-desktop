param([string]$Aad = '', [string]$InputHelper = '', [string]$BuildDirectory = '', [switch]$BuildOnly,
      [switch]$StraightGeometryOnly, [switch]$ExpectIncompatibleStraightDrag)
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $BuildDirectory) { $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-qt-path-' + [guid]::NewGuid().ToString('N')) }
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$fixturePath = Join-Path $BuildDirectory 'qt-path-fixture'
$flags = ((& pkg-config --cflags --libs Qt5Widgets) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'Qt5Widgets development packages unavailable' }
& g++ -std=c++17 -fPIC -O2 -Wall -Wextra -Werror (Join-Path $root 'tests/linux/qt_drag_path_fixture.cpp') -o $fixturePath @flags
if ($LASTEXITCODE -ne 0) { throw 'Qt path fixture build failed' }
if ($BuildOnly) { return }
if (-not $Aad -or -not $InputHelper) { throw 'Aad and InputHelper are required' }
$script:server = $null; $script:sessionId = $null; $script:sequence = 0; $fixture = $null
$script:responses = @{}; $checks = [Collections.Generic.List[object]]::new()
$script:logPath = Join-Path $BuildDirectory 'native-path.ndjson'
function Send-Control([hashtable]$operation) {
    $script:sequence++; $id = "path-$script:sequence"
    $request = @{request_id=$id; session_id=$script:sessionId; timeout_ms=20000; operation=$operation}
    $script:server.StandardInput.WriteLine(($request | ConvertTo-Json -Depth 40 -Compress)); $script:server.StandardInput.Flush()
    if ($operation.method -ne 'targets') { @{request=$request} | ConvertTo-Json -Depth 50 -Compress | Add-Content -Encoding UTF8 $script:logPath }
    return $id
}
function Receive-Control([string]$id) {
    while (-not $script:responses.ContainsKey($id)) {
        $line = $script:server.StandardOutput.ReadLineAsync()
        if (-not $line.Wait(22000) -or -not $line.Result) { throw 'control response missing' }
        $response = $line.Result | ConvertFrom-Json; $script:sessionId = $response.session_id
        $script:responses[$response.request_id] = $response
        if (-not $response.data.resources) { @{response=$response} | ConvertTo-Json -Depth 50 -Compress | Add-Content -Encoding UTF8 $script:logPath }
    }
    $response = $script:responses[$id]; $script:responses.Remove($id); return $response
}
function Call-Control([hashtable]$operation) { return Receive-Control (Send-Control $operation) }
function Fixture-Event {
    $line = $fixture.StandardOutput.ReadLineAsync()
    if (-not $line.Wait(10000) -or -not $line.Result -or -not $line.Result.StartsWith('PATH ')) { throw 'independent fixture event missing' }
    $event = $line.Result.Substring(5) | ConvertFrom-Json
    @{case=$script:caseName;fixture_event=$event;elapsed_ms=$script:caseClock.ElapsedMilliseconds} | ConvertTo-Json -Depth 10 -Compress | Add-Content -Encoding UTF8 $script:logPath
    return $event
}
try {
    $info = [Diagnostics.ProcessStartInfo]::new(); $info.FileName = (Resolve-Path $Aad).Path
    $info.Arguments='control serve --allow-input'; $info.UseShellExecute=$false
    $info.RedirectStandardInput=$true; $info.RedirectStandardOutput=$true
    $info.EnvironmentVariables['AAD_LINUX_XTEST_HELPER']=(Resolve-Path $InputHelper).Path
    $script:server=[Diagnostics.Process]::Start($info)
    Call-Control @{method='capabilities'} | Out-Null
    $cases = if ($StraightGeometryOnly -or $ExpectIncompatibleStraightDrag) { @('geometry-straight') } else {
        @('invalid','success','success-scoped','geometry','overlay','overlay-scoped','cancel','geometry-straight',
          'overlay-straight','transparent-straight')
    }
    foreach ($caseName in $cases) {
        $script:caseName = $caseName
        $mode = $caseName.Replace('-scoped','')
        $fi=[Diagnostics.ProcessStartInfo]::new(); $fi.FileName=$fixturePath; $fi.Arguments=$mode
        $fi.UseShellExecute=$false; $fi.RedirectStandardOutput=$true
        $fixture=[Diagnostics.Process]::Start($fi)
        $ready=$fixture.StandardOutput.ReadLineAsync()
        if (-not $ready.Wait(10000) -or $ready.Result -ne 'READY') { throw 'fixture startup failed' }
        $found=@()
        for ($i=0;$i -lt 30;$i++) {
            $targets=Call-Control @{method='targets'}
            $found=@($targets.data.resources | Where-Object { $_.metadata.process_id -eq $fixture.Id })
            if ($found.Count -eq 1) { break }; Start-Sleep -Milliseconds 100
        }
        if ($found.Count -ne 1) { throw 'fixture resource missing' }
        $rid=$found[0].resource_id
        $observed=Call-Control @{method='observe';resource_id=$rid;max_nodes=100;view=@{mode='nodes'}}
        if ($observed.error -or $observed.data.truncated) { throw 'fixture capture failed' }
        $nodes=@($observed.data.nodes | Where-Object { $_.name -eq 'Path surface' })
        if ($nodes.Count -ne 1) { throw 'path surface missing or ambiguous' }
        $target=$nodes[0].target
        if ($caseName.EndsWith('-scoped')) {
            $observed=Call-Control @{method='observe';resource_id=$rid;max_nodes=100;view=@{mode='nodes';scope_target=$target}}
            if ($observed.error -or $observed.data.truncated) { throw 'surface scope capture failed' }
            $nodes=@($observed.data.nodes | Where-Object { $_.name -eq 'Path surface' })
            if ($nodes.Count -ne 1) { throw 'scoped surface missing or ambiguous' }
            $target=$nodes[0].target
        }
        $action=@{type='drag';to=$target;fraction=@{fx=0.1;fy=0.1};to_fraction=@{fx=0.9;fy=0.9};
            path=@(@{target=$target;fraction=@{fx=0.8;fy=0.2}},@{target=$target;fraction=@{fx=0.2;fy=0.8}})}
        if ($mode -eq 'invalid') { $action.path[0].fraction.fx=1.1 }
        if ($mode.EndsWith('-straight')) {
            $action=@{type='drag';to=$target;fraction=@{fx=0.3;fy=0.3};to_fraction=@{fx=0.7;fy=0.7};hold_ms=250;path=@()}
        }
        $script:caseClock=[Diagnostics.Stopwatch]::StartNew()
        $running=Send-Control @{method='act';target=$target;action=$action}
        if ($ExpectIncompatibleStraightDrag) {
            $result=Receive-Control $running
            $pending=$fixture.StandardOutput.ReadLineAsync()
            if ($pending.Wait(250)) { throw "incompatible helper sent input to the surface or cover: $($pending.Result)" }
            if ($result.effect -ne 'not_applied' -or $result.error.code -ne 'DRIVER.HELPER_INCOMPATIBLE') {
                throw "expected incompatible helper before dispatch, got $($result.effect) / $($result.error.code)"
            }
            if ($result.error.message -notlike '*Upgrade aad and its helper together.') {
                throw "expected an aad and helper upgrade hint, got: $($result.error.message)"
            }
            $checks.Add(@{case=$caseName;effect=$result.effect;code=$result.error.code;
                message=$result.error.message;events=@()})
        } elseif ($mode -eq 'invalid') {
            $result=Receive-Control $running
            if (-not $result.error -or $result.effect -ne 'not_applied') { throw 'bad path was not rejected before dispatch' }
            $observed=Call-Control @{method='observe';resource_id=$rid;max_nodes=100;view=@{mode='nodes'}}
            if (@($observed.data.nodes | Where-Object { $_.name -eq 'Path idle' }).Count -ne 1) { throw 'bad path changed fixture state' }
            $checks.Add(@{case=$caseName;effect=$result.effect;zero_down=$true;elapsed_ms=$script:caseClock.ElapsedMilliseconds})
        } else {
            $events=[Collections.Generic.List[object]]::new(); $events.Add((Fixture-Event))
            if ($events[0].event -ne 'down') { throw 'path did not start with down' }
            $clock=[Diagnostics.Stopwatch]::StartNew()
            if ($mode -eq 'cancel') { Call-Control @{method='cancel';request_id=$running} | Out-Null }
            do { $event=Fixture-Event; $events.Add($event) } while ($event.event -ne 'up')
            $releaseMs=$clock.ElapsedMilliseconds; $result=Receive-Control $running
            $moves=@($events | Where-Object { $_.event -eq 'move' })
            if ($mode -eq 'success') {
                if ($result.error -or $result.effect -ne 'applied' -or $moves.Count -ne 3) { throw 'successful drag did not deliver all anchors' }
                $expected=@(@(0.8,0.2),@(0.2,0.8),@(0.9,0.9))
                for ($i=0;$i -lt 3;$i++) {
                    if ([Math]::Abs($moves[$i].fx-$expected[$i][0]) -gt 0.015 -or [Math]::Abs($moves[$i].fy-$expected[$i][1]) -gt 0.015) { throw 'actual pointer trace differs from normalized anchors' }
                }
            } elseif ($mode -eq 'geometry-straight') {
                $extra=$fixture.StandardOutput.ReadLineAsync()
                if ($extra.Wait(150)) { throw "unexpected event after release: $($extra.Result)" }
                $moved=Call-Control @{method='observe';resource_id=$rid;max_nodes=100;view=@{mode='nodes'}}
                $movedSurface=@($moved.data.nodes | Where-Object { $_.name -eq 'Path surface' })
                if ($movedSurface.Count -ne 1 -or $movedSurface[0].bounds.x -eq $nodes[0].bounds.x) {
                    throw 'straight drag fixture window did not move'
                }
                if ($result.effect -ne 'unknown' -or $result.error.code -ne 'DRIVER.POINT_NOT_PROVEN' -or
                    ($events.event -join ',') -ne 'down,up' -or $releaseMs -gt 1500) {
                    throw "moving straight drag delivered $($events.event -join ',') with effect $($result.effect) and code $($result.error.code)"
                }
            } elseif ($mode -eq 'overlay-straight') {
                $extra=$fixture.StandardOutput.ReadLineAsync()
                if ($extra.Wait(150)) { throw "unexpected event after release: $($extra.Result)" }
                if ($result.effect -ne 'unknown' -or $result.error.code -ne 'DRIVER.POINT_NOT_PROVEN' -or
                    $moves.Count -ne 5 -or $event.fx -ne $moves[-1].fx -or $event.fy -ne $moves[-1].fy -or $releaseMs -gt 1500) {
                    throw "covered straight drag delivered $($events.event -join ',') with effect $($result.effect) and code $($result.error.code)"
                }
            } elseif ($mode -eq 'transparent-straight') {
                if ($result.effect -ne 'applied' -or $moves.Count -ne 12 -or
                    $event.fx -ne 0.7 -or $event.fy -ne 0.7) {
                    throw "input-transparent window interrupted straight drag: $($events.event -join ',')"
                }
            } else {
                if (-not $result.error -or $result.effect -ne 'unknown') { throw 'interrupted path did not preserve uncertain effect' }
                if ($mode -eq 'cancel' -and $result.error.code -notmatch 'CANCELLED$') { throw 'cancel did not propagate' }
                if ($releaseMs -gt 1500) { throw 'path release waited for full deadline' }
            }
            $checks.Add(@{case=$caseName;effect=$result.effect;code=$result.error.code;events=@($events);release_ms=$releaseMs;elapsed_ms=$script:caseClock.ElapsedMilliseconds})
        }
        $fixture.Kill(); $fixture.WaitForExit(); $fixture.Dispose(); $fixture=$null
    }
    @{passed=$true;checks=@($checks);helper_sha256=(Get-FileHash $InputHelper).Hash} | ConvertTo-Json -Depth 20 | Set-Content -Encoding UTF8 (Join-Path $BuildDirectory 'summary.json')
    Write-Output 'PASS: Qt independently observed every normalized path anchor, zero-input refusal and release on geometry/overlay/cancel'
} finally {
    if ($script:server) {
        try { if (-not $script:server.HasExited) { Call-Control @{method='close'} | Out-Null; $script:server.StandardInput.Close(); if (-not $script:server.WaitForExit(2000)) { $script:server.Kill() } } } catch { if (-not $script:server.HasExited) { $script:server.Kill() } }
        $script:server.Dispose()
    }
    if ($fixture) { if (-not $fixture.HasExited) { $fixture.Kill(); $fixture.WaitForExit() }; $fixture.Dispose() }
}
