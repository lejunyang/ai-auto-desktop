# 把 Windows 桌面 Excel 的未保存数据导出为 CSV

要读已经录入工作表、但还没保存到磁盘的值，指定现有 Excel 工作簿窗口的 HWND、工作簿的准确名称、工作表名称和 A1 区域。例如，`Book1` 中的 `Ledger` 已经录入本次对账数据，可以这样导出。

```powershell
aad csv export --excel-window 0x123456 --book-name Book1 --sheet Ledger --range 'B2:D24' --output ledger.csv
```

将示例 HWND 换成实际工作簿窗口的句柄。可用 `Get-Process EXCEL | Select-Object Id, MainWindowHandle, MainWindowTitle` 查看每个进程的主窗口。一个进程有多份工作簿时，需使用目标工作簿窗口自己的 HWND。参数接受十进制、`0x` 十六进制和 `hwnd:<十进制>`。工作簿名称对应 Excel 的 `Workbook.Name`，已命名的工作簿通常包含扩展名。输出文件必须是新文件。已有磁盘文件可使用 [本地 XLSX 导出](workbook-csv.md)。

命令从指定 `XLMAIN` 窗口找到同一 Excel 进程中的可见 `EXCEL7` 文档窗格，用 Office NativeOM 取得该窗格的 Excel Window。读取前后都会核对窗口句柄、PID、工作簿和工作表对象。另一个工作簿在前台时，命令仍以指定窗口为准。这个读取过程无需启动 Excel、切换工作表或保存工作簿。[Microsoft 的 NativeOM 窗口映射](https://learn.microsoft.com/en-us/windows/win32/api/oleacc/nf-oleacc-accessibleobjectfromwindow)

## CSV 中的值

- 第一行也是指定区域的数据。空单元格是空字段，存成文本的前导零会保留，布尔值写为 `true` 或 `false`。
- 公式导出 Excel 当前提供的计算结果。货币和日期使用 `Value2` 数值，例如日期可能是序列号；数字格式和公式文本不会导出。正在单元格编辑器中输入的文本需要先提交到工作表。[Excel Range.Value2](https://learn.microsoft.com/en-us/office/vba/api/excel.range.value2)
- 一次最多 320,000 个单元格，选中内容合计最多 16 MiB UTF-8 文本，每次 COM 调用读取至多 128 个值。多次读取没有事务隔离，持续编辑或重算可能使同一份 CSV 包含不同瞬间的值。
- 多个可见文档窗格、对象身份变化、合并单元格、Excel 错误值或读取超时都会使导出失败。读取完整并复核通过后才发布 CSV。

## Windows GUI 验证 [待验证]

需要装有桌面 Excel 2013 或更新版的交互式 Windows 会话。在独立测试桌面上运行以下命令。夹具会新建专用 Excel 实例和两份临时工作簿，让另一份工作簿保持前台，再从后台的未保存工作簿导出。它逐项核对 CSV、错误拒绝和未保存状态，最后关闭测试工作簿并放弃改动。

```powershell
cargo build --locked -p aad-cli --bin aad
pwsh -NoProfile -File tests/windows/verify-live-excel.ps1
```
