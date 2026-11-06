param([string]$Aad='', [string]$InputHelper='', [string]$BuildDirectory='', [switch]$BuildOnly)
$ErrorActionPreference='Stop'
$root=(Resolve-Path (Join-Path $PSScriptRoot '../..')).Path
if(-not $BuildDirectory){$BuildDirectory=Join-Path ([IO.Path]::GetTempPath()) ('aad-native-cross-'+[guid]::NewGuid().ToString('N'))}
New-Item -ItemType Directory -Path $BuildDirectory -Force | Out-Null
$BuildDirectory=(Resolve-Path $BuildDirectory).Path
$binary=Join-Path $BuildDirectory 'qt-cross-drag-fixture'
$flags=((& pkg-config --cflags --libs Qt5Widgets)-split '\s+')|Where-Object{$_}
if($LASTEXITCODE-ne 0){throw 'Qt5Widgets development packages unavailable'}
& g++ -std=c++17 -fPIC -O2 -Wall -Wextra -Werror (Join-Path $root 'tests/linux/qt_cross_drag_fixture.cpp') -o $binary @flags
if($LASTEXITCODE-ne 0){throw 'Qt cross-resource fixture build failed'}
if($BuildOnly){return}
if(-not $Aad-or-not $InputHelper){throw 'Aad and InputHelper required'}
$script:server=$null;$script:session=$null;$script:sequence=0;$script:responses=@{};$source=$null;$destination=$null
$script:logPath=Join-Path $BuildDirectory 'native-cross.ndjson';$checks=[Collections.Generic.List[object]]::new()
function Send-Control([hashtable]$operation){
    $script:sequence++;$id="cross-$script:sequence";$request=@{request_id=$id;session_id=$script:session;timeout_ms=20000;operation=$operation}
    $script:server.StandardInput.WriteLine(($request|ConvertTo-Json -Depth 40 -Compress));$script:server.StandardInput.Flush()
    if($operation.method-ne 'targets'){@{request=$request}|ConvertTo-Json -Depth 50 -Compress|Add-Content -Encoding UTF8 $script:logPath};return $id
}
function Receive-Control([string]$id){
    while(-not $script:responses.ContainsKey($id)){
        $line=$script:server.StandardOutput.ReadLineAsync();if(-not $line.Wait(22000)-or-not $line.Result){throw 'control response missing'}
        $response=$line.Result|ConvertFrom-Json;$script:session=$response.session_id;$script:responses[$response.request_id]=$response
        if(-not $response.data.resources){@{response=$response}|ConvertTo-Json -Depth 50 -Compress|Add-Content -Encoding UTF8 $script:logPath}
    };$response=$script:responses[$id];$script:responses.Remove($id);return $response
}
function Call-Control([hashtable]$operation){return Receive-Control (Send-Control $operation)}
function Fixture-Line($process){
    $line=$process.StandardOutput.ReadLineAsync();if(-not $line.Wait(15000)-or-not $line.Result){throw 'independent fixture event missing'}
    @{case=$script:mode;fixture_pid=$process.Id;event=$line.Result;elapsed_ms=$script:clock.ElapsedMilliseconds}|ConvertTo-Json -Compress|Add-Content -Encoding UTF8 $script:logPath
    return $line.Result
}
function Start-Fixture([string]$role,[string]$mode){
    $info=[Diagnostics.ProcessStartInfo]::new();$info.FileName=$binary;$info.Arguments="$role $mode";$info.UseShellExecute=$false;$info.RedirectStandardOutput=$true
    $info.EnvironmentVariables['QT_SCALE_FACTOR']='1';$process=[Diagnostics.Process]::Start($info)
    $line=$process.StandardOutput.ReadLineAsync();if(-not $line.Wait(10000)-or $line.Result-ne 'READY'){throw 'fixture startup failed'};return $process
}
function Resource-For($process){
    for($i=0;$i-lt 30;$i++){
        $targets=Call-Control @{method='targets'};$resources=@($targets.data.resources|Where-Object{$_.metadata.process_id-eq $process.Id})
        if($resources.Count-eq 1){return $resources[0].resource_id};Start-Sleep -Milliseconds 100
    };throw 'fixture resource missing'
}
function Observe-Surface([string]$rid,[string]$name){
    $observed=Call-Control @{method='observe';resource_id=$rid;max_nodes=100;view=@{mode='nodes'}}
    if($observed.error-or $observed.data.truncated){throw 'fixture observation failed'}
    $nodes=@($observed.data.nodes|Where-Object{$_.name-eq $name});if($nodes.Count-ne 1){throw 'fixture surface missing or ambiguous'};return $nodes[0].target
}
try{
    $info=[Diagnostics.ProcessStartInfo]::new();$info.FileName=(Resolve-Path $Aad).Path;$info.Arguments='control serve --allow-input';$info.UseShellExecute=$false;$info.RedirectStandardInput=$true;$info.RedirectStandardOutput=$true
    $info.EnvironmentVariables['AAD_LINUX_XTEST_HELPER']=(Resolve-Path $InputHelper).Path;$script:server=[Diagnostics.Process]::Start($info)
    Call-Control @{method='capabilities'}|Out-Null
    foreach($mode in @('success','cancel','geometry','hover')){
        $script:mode=$mode;$source=Start-Fixture 'source' $mode;$destination=Start-Fixture 'destination' $mode
        $sourceRid=Resource-For $source;$destinationRid=Resource-For $destination
        $from=Observe-Surface $sourceRid 'Cross source';$to=Observe-Surface $destinationRid 'Cross destination'
        $action=@{type='drag';to=$to;fraction=@{fx=0.2;fy=0.3};to_fraction=@{fx=0.8;fy=0.3}}
        if($mode-eq 'cancel'){$action.hold_ms=500}
        $script:clock=[Diagnostics.Stopwatch]::StartNew();$running=Send-Control @{method='act';target=$from;action=$action}
        $first=Fixture-Line $source
        if($mode-eq 'hover'){
            if($first-ne 'HOVER'){throw 'hover obstacle did not appear before down'}
            $result=Receive-Control $running
            if(-not $result.error-or $result.effect-ne 'unknown'){throw 'hover obstacle did not stop dispatch'}
            $fresh=Call-Control @{method='observe';resource_id=$sourceRid;max_nodes=100;view=@{mode='nodes'}}
            if(@($fresh.data.nodes|Where-Object{$_.name-eq 'Cross source idle'}).Count-ne 1){throw 'hover refusal delivered button down'}
            $checks.Add(@{case=$mode;zero_button_down=$true;effect=$result.effect;code=$result.error.code;elapsed_ms=$script:clock.ElapsedMilliseconds})
        }else{
            if($first-ne 'PRESS'){throw 'source did not independently receive button down'}
            $releaseClock=[Diagnostics.Stopwatch]::StartNew()
            if($mode-eq 'cancel'){Call-Control @{method='cancel';request_id=$running}|Out-Null}
            $done=Fixture-Line $source;$result=Receive-Control $running
            if($mode-eq 'success'){
                if($done-ne 'DONE accepted'-or $result.error-or $result.effect-ne 'applied'){throw 'Qt cross-process drag did not complete'}
                if((Fixture-Line $destination)-ne 'DROP begin'){throw 'destination did not independently receive drop'}
                if((Fixture-Line $destination)-ne 'DROP matched'){throw 'destination did not independently receive exact MIME text'}
                $to=Observe-Surface $destinationRid 'Cross destination'
                $fresh=Call-Control @{method='observe';resource_id=$destinationRid;max_nodes=100;view=@{mode='nodes'}}
                if(@($fresh.data.nodes|Where-Object{$_.name-eq 'Cross destination received'}).Count-ne 1){throw 'destination visible result missing'}
            }else{
                if($done-ne 'UP'-or-not $result.error-or $result.effect-ne 'unknown'){throw 'interrupted drag did not release source'}
                if($mode-eq 'cancel'-and $result.error.code-notmatch 'CANCELLED$'){throw 'native cancellation did not propagate'}
                if($releaseClock.ElapsedMilliseconds-gt 1500){throw 'release waited for full request deadline'}
            }
            $checks.Add(@{case=$mode;effect=$result.effect;code=$result.error.code;source_result=$done;release_ms=$releaseClock.ElapsedMilliseconds;elapsed_ms=$script:clock.ElapsedMilliseconds})
        }
        $source.Kill();$source.WaitForExit();$source.Dispose();$source=$null;$destination.Kill();$destination.WaitForExit();$destination.Dispose();$destination=$null
    }
    @{passed=$true;checks=@($checks);helper_sha256=(Get-FileHash $InputHelper).Hash}|ConvertTo-Json -Depth 20|Set-Content -Encoding UTF8 (Join-Path $BuildDirectory 'summary.json')
    Write-Output 'PASS: two independent Qt processes transferred MIME data through public native drag; cancel/geometry/hover refused safely'
}finally{
    if($script:server){try{if(-not $script:server.HasExited){Call-Control @{method='close'}|Out-Null;$script:server.StandardInput.Close();if(-not $script:server.WaitForExit(2000)){$script:server.Kill()}}}catch{if(-not $script:server.HasExited){$script:server.Kill()}};$script:server.Dispose()}
    foreach($process in @($source,$destination)){if($process){if(-not $process.HasExited){$process.Kill();$process.WaitForExit()};$process.Dispose()}}
}
