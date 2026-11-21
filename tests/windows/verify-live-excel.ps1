# Run on an isolated, interactive Windows desktop with installed Excel 2013+.
param([string]$Aad = (Join-Path $PSScriptRoot '..\..\target\debug\aad.exe'))
$ErrorActionPreference = 'Stop'
$PSNativeCommandUseErrorActionPreference = $false # Check the deliberately failing CLI calls below.
if (Get-Process EXCEL -ErrorAction SilentlyContinue) {
    throw 'Use a test desktop without a running Excel instance.'
}
$tmp = New-Item -ItemType Directory -Path (Join-Path ([IO.Path]::GetTempPath()) ('aad-live-excel-' + [guid]::NewGuid()))
$excel = $null
$book = $null
$other = $null
try {
    # Only this fixture starts Excel. Both fixture books stay unsaved.
    $excel = New-Object -ComObject Excel.Application
    if ($excel.Workbooks.Count -ne 0) { throw 'Expected an empty fixture instance.' }
    $excel.Visible = $true
    $book = $excel.Workbooks.Add()
    $sheet = $book.Worksheets.Item(1)
    $sheet.Name = 'Unsaved ledger'
    $sheet.Range('B2').Value2 = 'ID'
    $sheet.Range('C2').Value2 = 'Amount'
    $sheet.Range('B3').NumberFormat = '@'
    $sheet.Range('B3').Value2 = '0007'
    $sheet.Range('C3').Formula = '=6*7'
    $sheet.Range('B4').Value2 = 'line, "quoted"'
    $sheet.Range('C4').Value2 = 73.25
    $sheet.Range('C4').NumberFormat = '0.000'
    $sheet.Range('D2').Value2 = 'flag'
    $sheet.Range('D3').Formula = '=TRUE()'
    $sheet.Range('F2:G2').Merge()
    $sheet.Range('H2').Formula = '=NA()'
    $hwnd = $book.Windows.Item(1).Hwnd
    $other = $excel.Workbooks.Add()
    $other.Worksheets.Item(1).Name = $sheet.Name
    $other.Worksheets.Item(1).Range('B3').Value2 = 'wrong workbook'
    if ($excel.ActiveWorkbook.Name -eq $book.Name -or $other.Windows.Item(1).Hwnd -eq $hwnd) {
        throw 'The test needs distinct workbook windows and the second book in front.'
    }
    $out = Join-Path $tmp 'live.csv'
    function Export-Live($windowId, $bookName, $sheetName, $address, $destination) {
        & $Aad csv export --excel-window "$windowId" --book-name $bookName --sheet $sheetName --range $address --output $destination | Out-Host
    }
    Export-Live $hwnd $book.Name $sheet.Name 'B2:C4' $out
    if ($LASTEXITCODE -ne 0) { throw 'Live range export failed.' }
    $rows = @(Import-Csv -Encoding UTF8 -LiteralPath $out)
    if ($rows.Count -ne 2 -or $rows[0].ID -cne '0007' -or $rows[0].Amount -ne '42' -or
        $rows[1].ID -cne $sheet.Range('B4').Value2 -or
        $rows[1].Amount -cne $sheet.Range('C4').Value2.ToString([cultureinfo]::InvariantCulture)) {
        throw 'CSV did not match the unsaved sheet values.'
    }
    $wide = Join-Path $tmp 'wide.csv'
    Export-Live $hwnd $book.Name $sheet.Name 'B2:D3' $wide
    if ($LASTEXITCODE -ne 0 -or (Import-Csv -Encoding UTF8 -LiteralPath $wide).flag -cne 'true') { throw 'Horizontal strip failed.' }
    $single = Join-Path $tmp 'cell.csv'
    Export-Live $hwnd $book.Name $sheet.Name 'C3' $single
    if ($LASTEXITCODE -ne 0 -or [IO.File]::ReadAllText($single).TrimEnd() -cne '42') { throw 'Scalar Value2 failed.' }
    $denied = Join-Path $tmp 'denied.csv'
    foreach ($bad in @(
        @($hwnd, $other.Name, $sheet.Name, 'B2:C4'),
        @($other.Windows.Item(1).Hwnd, $book.Name, $sheet.Name, 'B2:C4'),
        @($hwnd, $book.Name, 'Missing worksheet', 'A1'),
        @($hwnd, $book.Name, $sheet.Name, 'F2:G2'),
        @($hwnd, $book.Name, $sheet.Name, 'H2'),
        @($hwnd, $book.Name, $sheet.Name, 'A1:XFD1048576')
    )) {
        Export-Live $bad[0] $bad[1] $bad[2] $bad[3] $denied
        if ($LASTEXITCODE -eq 0 -or (Test-Path $denied)) { throw 'Unsafe or incomplete export succeeded.' }
    }
    if ($book.Saved -or $book.Path -or $excel.ActiveWorkbook.Name -ne $other.Name) {
        throw 'The reader saved or activated the source workbook.'
    }
    Write-Output 'Live Excel HWND, unsaved values, shape and refusal checks passed.'
} finally {
    if ($null -ne $other) { $other.Close($false) }
    if ($null -ne $book) { $book.Close($false) }
    if ($null -ne $excel -and $excel.Workbooks.Count -eq 0) { $excel.Quit() }
    Remove-Item -LiteralPath $tmp -Recurse -Force
}
