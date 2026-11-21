# 将磁盘工作簿的指定区域导出为 CSV

`aad csv export --workbook` 以只读方式打开已保存或下载到本机的 `.xlsx` 文件。公式取工作簿内的缓存结果。采集结果 JSON 使用 `--input collection.json`。Windows 桌面 Excel 中尚未保存的数据有[按窗口读取的方法](live-excel-csv.md)。

```powershell
aad csv export --workbook ./月账单.xlsx --sheet '账单' --range 'B2:E101' --output ./expected.csv
aad csv compare --input ./网页账单.json --csv ./expected.csv --decimal Amount
```

工作表名须精确匹配。区域支持 `B2`、`B2:E101` 或 `$B$2:$E$101`；PowerShell 中用单引号防止展开 `$`。输出按指定矩形逐行写入，不跳过空行、不推断表头、不展开合并单元格。用于对账时，应让区域首行正好是与网页账单对应的列名，并提供唯一、非空的行键。

CSV 使用 UTF-8 和 CRLF，保留文本单元格的前导零、引号、逗号和换行。数值写原值，不复刻 Excel 的显示格式；数值日期写工作簿序列值，ISO 日期字符串保持原文。公式只读文件中的缓存结果，没有缓存时不计算。工作簿不变，目标 CSV 必须是新文件。

上限：输入普通文件 16 MiB；ZIP 最多 4096 个成员，单成员展开不超过 16 MiB、总展开不超过 64 MiB；共享字符串表声明的唯一值不超过 100 万。区域最多 32 万个单元格，选中单元格的文本合计不超过 16 MiB。不支持跨表区域、整行整列引用、加密工作簿或旧版 `.xls`。
