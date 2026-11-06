param(
    [Parameter(Mandatory = $true)][string]$Aad,
    [Parameter(Mandatory = $true)][string]$InputHelper,
    [Parameter(Mandatory = $true)][string]$CaptureHelper,
    [string]$BuildDirectory = ''
)

# Run alone on an authorized interactive X11 desktop. All input goes to our Qt
# fixture, and only equality/empty booleans leave that process.
$ErrorActionPreference = 'Stop'
$root = (Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if (-not $BuildDirectory) {
    $BuildDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-secret-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory = (Resolve-Path $BuildDirectory).Path
$fixturePath = Join-Path $BuildDirectory 'qt-secret-fixture'
$flags = ((& pkg-config --cflags --libs Qt5Widgets) -split '\s+') | Where-Object { $_ }
if ($LASTEXITCODE -ne 0) { throw 'Qt5Widgets development packages unavailable' }
& g++ -std=c++17 -fPIC -O2 -Wall -Wextra -Werror `
    (Join-Path $root 'tests/linux/qt_secret_fixture.cpp') -o $fixturePath @flags
if ($LASTEXITCODE -ne 0) { throw 'Qt secret fixture build failed' }

$synthetic = 'Synthetic secret 中文 7x'
$script:server = $null
$script:sessionId = $null
$script:resourceId = $null
$fixture = $null
$script:sequence = 0
$checks = [Collections.Generic.List[string]]::new()

function New-StartInfo([string]$program, [string]$arguments) {
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = $program
    $info.Arguments = $arguments
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.EnvironmentVariables['AAD_LINUX_XTEST_HELPER'] = (Resolve-Path $InputHelper).Path
    $info.EnvironmentVariables['AAD_LINUX_CAPTURE_HELPER'] = (Resolve-Path $CaptureHelper).Path
    return $info
}

function Call-Control([hashtable]$operation) {
    $script:sequence++
    $request = @{request_id = "secret-test-$script:sequence"; session_id = $script:sessionId; timeout_ms = 12000; operation = $operation}
    $script:server.StandardInput.WriteLine(($request | ConvertTo-Json -Depth 30 -Compress))
    $script:server.StandardInput.Flush()
    $pending = $script:server.StandardOutput.ReadLineAsync()
    if (-not $pending.Wait(15000)) { throw 'control response deadline exceeded' }
    $line = $pending.Result
    if (-not $line) { throw 'control server stopped' }
    if ($line.Contains($synthetic)) { throw 'secret appeared in a public response' }
    $response = $line | ConvertFrom-Json
    $script:sessionId = $response.session_id
    return $response
}

function Observe-Fixture {
    $result = Call-Control @{method = 'observe'; resource_id = $script:resourceId; max_nodes = 100; view = @{mode = 'nodes'}}
    if ($result.error -or $result.data.truncated) { throw 'fixture observation failed or truncated' }
    return $result.data
}

function Protected-Field {
    $fields = @((Observe-Fixture).nodes | Where-Object { $_.states.protected -eq $true })
    if ($fields.Count -ne 1 -or $null -ne $fields[0].value) { throw 'Qt password field was not identified and redacted' }
    return $fields[0]
}

function Stop-Server {
    if ($script:server -and -not $script:server.HasExited) {
        try { Call-Control @{method = 'close'} | Out-Null } finally {
            $script:server.StandardInput.Close()
            if (-not $script:server.WaitForExit(15000)) { $script:server.Kill() }
        }
    }
    $script:sessionId = $null
}

function Start-Server([bool]$secretGrant) {
    $arguments = 'control serve --allow-input'
    if ($secretGrant) { $arguments += ' --allow-secret-input --secret-env login=AAD_SECRET_TEST_VALUE --secret-env empty=AAD_SECRET_TEST_EMPTY' }
    $info = New-StartInfo (Resolve-Path $Aad).Path $arguments
    $info.EnvironmentVariables['AAD_SECRET_TEST_VALUE'] = $synthetic
    $info.EnvironmentVariables['AAD_SECRET_TEST_EMPTY'] = ''
    $script:server = [Diagnostics.Process]::Start($info)
    Call-Control @{method = 'capabilities'} | Out-Null
    $found = $null
    for ($i = 0; $i -lt 20; $i++) {
        $targets = Call-Control @{method = 'targets'}
        $found = @($targets.data.resources | Where-Object { $_.metadata.process_id -eq $fixture.Id })
        if ($found.Count -eq 1) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($found.Count -ne 1) { throw 'fixture resource not found' }
    $script:resourceId = $found[0].resource_id
}

function Check-Fixture([string]$buttonName, [string]$expectedStatus) {
    $button = @((Observe-Fixture).nodes | Where-Object { $_.name -eq $buttonName -and $_.capabilities -contains 'invoke' })
    if ($button.Count -ne 1) { throw 'fixture check button missing' }
    $result = Call-Control @{method = 'act'; target = $button[0].target; action = @{type = 'invoke'}}
    if ($result.effect -ne 'applied') { throw 'fixture check invocation failed' }
    for ($i = 0; $i -lt 20; $i++) {
        Start-Sleep -Milliseconds 100
        if (@((Observe-Fixture).nodes | Where-Object { $_.name -eq $expectedStatus }).Count -gt 0) { return }
    }
    throw 'independent fixture check did not match'
}

try {
    $fixtureInfo = New-StartInfo $fixturePath ''
    $fixtureInfo.EnvironmentVariables['AAD_SECRET_FIXTURE_EXPECTED'] = $synthetic
    $fixture = [Diagnostics.Process]::Start($fixtureInfo)
    $ready = $fixture.StandardOutput.ReadLineAsync()
    if (-not $ready.Wait(10000) -or $ready.Result -ne 'READY') { throw 'fixture failed to start' }
    Start-Server $false
    $field = Protected-Field
    $denied = Call-Control @{method = 'act'; target = $field.target; action = @{type = 'secret_input'; secret_ref = 'login'}}
    if ($denied.error.code -ne 'POLICY.DENIED') { throw 'default secret grant was not denied' }
    $checks.Add('default_denied')
    Stop-Server
    Start-Server $true
    $field = Protected-Field
    if ($field.capabilities -notcontains 'secret_input') { throw 'authorized secret action unavailable' }
    $denied = Call-Control @{method = 'act'; target = $field.target; action = @{type = 'fill'; value = 'ordinary-text'}}
    if ($denied.error.code -ne 'POLICY.DENIED') { throw 'ordinary fill bypassed secret policy' }
    $checks.Add('ordinary_fill_denied')
    foreach ($binding in @('login', 'empty')) {
        $field = Protected-Field
        $result = Call-Control @{method = 'act'; target = $field.target; action = @{type = 'secret_input'; secret_ref = $binding}}
        $backend = $result.data.backend_result
        if ($result.effect -ne 'applied' -or -not $backend.submitted -or -not $backend.redacted -or @($backend.PSObject.Properties).Count -ne 2) { throw 'secret input failed or returned excess data' }
        if ($binding -eq 'login') { Check-Fixture 'Check secret replacement' 'Secret replacement matched' }
        else { Check-Fixture 'Check secret cleared' 'Secret cleared' }
        $checks.Add("verified_$binding")
        $field = Protected-Field
        $capture = Call-Control @{method = 'capture'; target = $field.target}
        if (-not $capture.error) { throw 'protected field capture was allowed' }
    }
    @{passed = $true; checks = @($checks)} | ConvertTo-Json | Set-Content -Encoding UTF8 (Join-Path $BuildDirectory 'summary.json')
    Write-Output 'PASS: Qt password identity, grant, replacement, clearing, redaction and capture protection'
} finally {
    try { Stop-Server } finally {
        if ($fixture -and -not $fixture.HasExited) {
            if (-not $fixture.CloseMainWindow() -or -not $fixture.WaitForExit(2000)) { $fixture.Kill(); $fixture.WaitForExit() }
        }
    }
}
