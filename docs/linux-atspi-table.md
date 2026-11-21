# 从正在运行的 Linux 应用读取表格

`control_read_table` 可以读取应用通过 AT-SPI 暴露的当前单元格。编辑只要已经反映到表格的可访问文本中，就可以在保存文件之前读到。返回值是字符串，保留应用暴露的小数位、前导零和空文本。

## 按列读取

1. 启动 `aad mcp --provider desktop.linux_atspi`。这个读取操作使用 `desktop.observe` 权限，无需授予输入。
2. 用 `control_targets` 找到会话获准观察的应用资源。把它的 `resource_id` 传给 `control_observe`，用 `{"mode":"nodes"}` 作为 `view`。
3. 用 `control_find` 在这次观察中定位表格本身。例如传入观察返回的 `observation_id` 和 `{"name":"Unsaved grid"}`。把返回的完整 `target` 原样交给 `control_read_table`。

```json
{
  "target": {
    "session_id": "<会话 ID>",
    "resource_id": "<应用资源 ID>",
    "observation_id": "<观察 ID>",
    "node_id": "<表格节点 ID>"
  },
  "columns": ["ID", "Amount"],
  "max_characters": 20000
}
```

读取时会再次核对原作用域、应用、原生对象和节点身份。每个请求的列名必须与一个可访问表头精确匹配。下面是 GTK3 测试在修改内存数据后返回的 `data`。

```json
{
  "scope": "atspi_table_rows",
  "records": [
    {"ID": "INV-01", "Amount": "73.25"},
    {"ID": "INV-02", "Amount": "19.00"}
  ],
  "row_count": 2,
  "truncated": false,
  "truncation_reasons": []
}
```

## 如何判断读了多少

- `row_count` 是读取时 AT-SPI Table 报告的行数，`records` 按该表的行顺序排列。应用的筛选、折叠、分页或虚拟化可能影响它暴露的行。驱动不会翻页、展开树或提交正在编辑的内容。
- 每次最多选 32 列，表格最多可有 128 列。结果最多包含 500 行、2,000 个选中的单元格。字符预算默认为 20,000，可设为 500 至 200,000，列名也占预算。达到结果上限时返回完整行的前缀，`truncated` 为 `true`，原因是 `max_characters` 或 `cell_limit`。对账时应先检查这个标记。
- 原观察或复核时的可访问树被截断、目标变更、表头有歧义、选中单元格受保护、跨度未知或涉及合并时，调用返回错误。数据单元格必须是同一应用、同一张表下的直接叶节点，并且提供 AT-SPI Text；带内嵌控件的复合单元格不在支持范围内。
- 每个选中单元格都会复核对象、文本长度和跨度，结束时再次读取表格尺寸和表头。AT-SPI 的多次读取没有事务隔离，若应用持续刷新，不同行的值可能来自不同瞬间。

## 实测范围

在隔离的 Xvfb 和 D-Bus 会话中，真实 GTK3 TreeView 的 ListStore 在观察后将金额从 `old` 改为 `73.25`。测试逐行比较 MCP 返回值与应用直接从内存模型读出的值，并验证了预算不足、受保护列、重复表头、资源不匹配、目标改名以及通过 ATK 声明合并跨度时的结果。

在有 GTK3、PyGObject、AT-SPI、Xvfb 和 D-Bus 的 Linux 环境中，用 PowerShell 7 从仓库根目录运行。私有 Xvfb 的临时文件使用当前 `TMPDIR`。

```powershell
cargo build --locked -p aad-cli --bin aad
dbus-run-session -- xvfb-run -a env -u AT_SPI_BUS_ADDRESS -u NO_AT_BRIDGE GDK_BACKEND=x11 GTK_A11Y=always /usr/bin/python3 tests/linux/atspi_table.py target/debug/aad
```

[待验证] Qt 表格、LibreOffice Calc 及原生 Wayland 会话需要各自在目标环境中验证表头、单元格文本与跨度。仍停留在编辑器内的草稿能否读取，取决于应用何时把它同步到表格的可访问文本。
