param(
    [Parameter(Mandatory = $true)][string]$Aad,
    [Parameter(Mandatory = $true)][string]$InputHelper,
    [string]$Dolphin = '/usr/bin/dolphin',
    [string]$OutputDirectory = ''
)

# Run alone on an authorized interactive X11 desktop. All mutations are confined
# to a new synthetic file and a new Dolphin window; existing windows are not used.
$ErrorActionPreference = 'Stop'
if (-not $OutputDirectory) {
    $OutputDirectory = Join-Path ([IO.Path]::GetTempPath()) ('aad-dolphin-rename-' + [guid]::NewGuid().ToString('N'))
}
New-Item -ItemType Directory -Path $OutputDirectory -Force | Out-Null
$OutputDirectory = (Resolve-Path $OutputDirectory).Path
$folder = Join-Path $OutputDirectory ('rename-fixture-' + [guid]::NewGuid().ToString('N'))
New-Item -ItemType Directory -Path $folder | Out-Null
$contents = "Synthetic AAD focus recovery fixture`n"
[IO.File]::WriteAllText((Join-Path $folder 'first.txt'), $contents)
$renamed = 'renamed-' + [char]0x4e2d + [char]0x6587 + '.txt'
$script:server = $null
$script:sessionId = $null
$script:resourceId = $null
$script:sequence = 0
$checks = [Collections.Generic.List[object]]::new()
$passed = $false

function Call-Control([hashtable]$operation) {
    $script:sequence++
    $request = @{request_id = "rename-test-$script:sequence"; session_id = $script:sessionId; timeout_ms = 12000; operation = $operation}
    $script:server.StandardInput.WriteLine(($request | ConvertTo-Json -Depth 30 -Compress))
    $script:server.StandardInput.Flush()
    $pending = $script:server.StandardOutput.ReadLineAsync()
    if (-not $pending.Wait(15000)) { throw 'control response deadline exceeded' }
    if (-not $pending.Result) { throw 'control server stopped' }
    $response = $pending.Result | ConvertFrom-Json
    $script:sessionId = $response.session_id
    # A target listing can contain unrelated applications. Only retain the
    # requested application's observations and actions on its temporary window.
    if ($operation.method -eq 'act' -or $operation.method -eq 'observe') {
        @{request = $request; response = $response} | ConvertTo-Json -Depth 50 -Compress | Add-Content -Encoding UTF8 (Join-Path $OutputDirectory 'rename.ndjson')
    }
    return $response
}

function Observe-Dolphin {
    $response = Call-Control @{method = 'observe'; resource_id = $script:resourceId; max_nodes = 2000; view = @{mode = 'nodes'}}
    if ($response.error -or $response.data.truncated) { throw 'Dolphin observation failed or truncated' }
    return $response.data
}

function Find-Editor($observation) {
    $found = @($observation.nodes | Where-Object {
        $_.provenance.focus_discovery -eq 'application_scoped_event' -and $_.states.focused -and $_.states.editable
    })
    if ($found.Count -gt 1) { throw 'recovered focused editor is ambiguous' }
    if ($found.Count -eq 1) { return $found[0] }
    return $null
}

function Require-Applied($response, [string]$operation) {
    if ($response.effect -ne 'applied') { throw "$operation failed: $($response.error.code)" }
    $checks.Add(@{case = $operation; effect = $response.effect})
}

try {
    $dolphinPath = (Resolve-Path $Dolphin).Path
    if ($dolphinPath.Contains('"')) { throw 'Dolphin executable path cannot contain a quote' }
    $info = [Diagnostics.ProcessStartInfo]::new()
    $info.FileName = (Resolve-Path $Aad).Path
    $info.Arguments = 'control serve --allow-input --allow-launch "' + $dolphinPath + '"'
    $info.UseShellExecute = $false
    $info.RedirectStandardInput = $true
    $info.RedirectStandardOutput = $true
    $info.EnvironmentVariables['AAD_LINUX_XTEST_HELPER'] = (Resolve-Path $InputHelper).Path
    $script:server = [Diagnostics.Process]::Start($info)
    Call-Control @{method = 'capabilities'} | Out-Null
    $launch = Call-Control @{method = 'launch'; program = $dolphinPath; args = @('--new-window', $folder); wait_ms = 5000}
    if ($launch.error) { throw "Dolphin launch failed: $($launch.error.code)" }
    $found = @()
    for ($i = 0; $i -lt 20; $i++) {
        $targets = Call-Control @{method = 'targets'}
        $found = @($targets.data.resources | Where-Object { $_.metadata.process_id -eq $launch.data.process_id -and $_.name -eq 'dolphin' })
        if ($found.Count -eq 1) { break }
        Start-Sleep -Milliseconds 100
    }
    if ($found.Count -ne 1) { throw 'new Dolphin process resource missing or ambiguous' }
    $script:resourceId = $found[0].resource_id
    $observation = Observe-Dolphin
    $files = @($observation.nodes | Where-Object { $_.role -eq 'cell' -and $_.name -eq 'first.txt' })
    if ($files.Count -ne 1) { throw 'synthetic file cell missing or ambiguous' }
    $file = $files[0]
    $surface = @($observation.nodes | Where-Object { $_.node_id -eq $file.parent_id })[0]
    $fraction = @{fx = ($file.bounds.x + $file.bounds.width / 2 - $surface.bounds.x) / $surface.bounds.width; fy = ($file.bounds.y + $file.bounds.height / 2 - $surface.bounds.y) / $surface.bounds.height}
    Require-Applied (Call-Control @{method = 'act'; target = $surface.target; action = @{type = 'click'; fraction = $fraction}}) 'select'
    $file = @((Observe-Dolphin).nodes | Where-Object { $_.role -eq 'cell' -and $_.name -eq 'first.txt' })[0]
    Require-Applied (Call-Control @{method = 'act'; target = $file.target; action = @{type = 'key'; keys = @('f2')}}) 'f2'
    $editor = $null
    for ($i = 0; $i -lt 10; $i++) {
        Start-Sleep -Milliseconds 150
        $observation = Observe-Dolphin
        $editor = Find-Editor $observation
        if ($editor) { break }
    }
    if (-not $editor) { throw 'focus event did not recover the actual editor' }
    $contexts = @($observation.nodes | Where-Object { $_.provenance.focus_context })
    foreach ($context in $contexts) {
        if ($context.capabilities.Count -ne 0 -or -not $context.provenance.scope_context) { throw 'recovered context granted actions' }
    }
    $checks.Add(@{case = 'recovered_editor'; role = $editor.role; focused = $editor.states.focused; editable = $editor.states.editable; context_count = $contexts.Count; contexts_read_only = $true})
    Require-Applied (Call-Control @{method = 'act'; target = $editor.target; action = @{type = 'fill'; value = $renamed}}) 'fill'
    $editor = Find-Editor (Observe-Dolphin)
    if (-not $editor -or $editor.value -ne $renamed) { throw 'editor readback differs from requested filename' }
    Require-Applied (Call-Control @{method = 'act'; target = $editor.target; action = @{type = 'key'; keys = @('enter')}}) 'commit'
    $destination = Join-Path $folder $renamed
    for ($i = 0; $i -lt 20 -and -not [IO.File]::Exists($destination); $i++) { Start-Sleep -Milliseconds 100 }
    if (-not [IO.File]::Exists($destination) -or [IO.File]::Exists((Join-Path $folder 'first.txt')) -or [IO.File]::ReadAllText($destination) -ne $contents) { throw 'independent disk verification failed' }
    $checks.Add(@{case = 'disk_readback'; new_name_exists = $true; old_name_absent = $true; contents_unchanged = $true})
    $passed = $true
    Write-Output 'PASS: observed Dolphin editor renamed a synthetic file; disk name and content independently verified'
} finally {
    if ($script:server -and -not $script:server.HasExited) {
        try {
            if ($script:resourceId) {
                $observation = Observe-Dolphin
                $frames = @($observation.nodes | Where-Object { $_.name -eq [IO.Path]::GetFileName($folder) -and $_.states.showing -and $_.capabilities -contains 'close_window' })
                if ($frames.Count -eq 1) { Call-Control @{method = 'act'; target = $frames[0].target; action = @{type = 'close_window'}} | Out-Null }
            }
        } finally {
            try { Call-Control @{method = 'close'} | Out-Null } finally {
                $script:server.StandardInput.Close()
                if (-not $script:server.WaitForExit(15000)) { $script:server.Kill(); $script:server.WaitForExit() }
            }
        }
    }
    @{passed = $passed; checks = @($checks); fixture = $folder; helper_sha256 = (Get-FileHash $InputHelper).Hash} | ConvertTo-Json -Depth 15 | Set-Content -Encoding UTF8 (Join-Path $OutputDirectory 'summary.json')
}
