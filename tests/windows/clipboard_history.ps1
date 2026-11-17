# Prints the texts in Windows' clipboard history (Win+V) as a JSON array, or
# {"status": ...} when it cannot be read. Windows PowerShell 5.1 (WinRT).
[Console]::OutputEncoding = [Text.Encoding]::UTF8
Add-Type -AssemblyName System.Runtime.WindowsRuntime
$asTask = [System.WindowsRuntimeSystemExtensions].GetMethods() | Where-Object {
    $_.Name -eq 'AsTask' -and $_.GetParameters().Count -eq 1 -and
    $_.GetParameters()[0].ParameterType.Name -eq 'IAsyncOperation`1'
} | Select-Object -First 1
function Await($operation, [Type]$type) {
    $task = $asTask.MakeGenericMethod($type).Invoke($null, @($operation))
    $task.Wait(-1) | Out-Null
    $task.Result
}
$null = [Windows.ApplicationModel.DataTransfer.Clipboard, Windows.ApplicationModel.DataTransfer, ContentType = WindowsRuntime]
$result = Await ([Windows.ApplicationModel.DataTransfer.Clipboard]::GetHistoryItemsAsync()) ([Windows.ApplicationModel.DataTransfer.ClipboardHistoryItemsResult])
if ($result.Status -ne 'Success') {
    [Console]::Out.WriteLine((@{status = "$($result.Status)"} | ConvertTo-Json -Compress))
    exit
}
$texts = @()
foreach ($item in $result.Items) {
    if ($item.Content.Contains('Text')) {
        $texts += Await ($item.Content.GetTextAsync()) ([string])
    }
}
[Console]::Out.WriteLine((ConvertTo-Json -InputObject @($texts) -Compress))
