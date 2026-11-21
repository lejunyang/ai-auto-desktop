# 收集表格、导出 CSV 和对账

网页采集支持 Chromium 有表头的 HTML 表格和声明了行、列的 ARIA 网格。金额、日期等值分散在页面上时，可以直接按字段读取。

运行 `aad browser --help` 可以找到页面列表、脚本、录制和网页表格采集。本地 CSV 文件处理归在 `aad csv`。已有的 `aad table collect/export/compare` 脚本仍可运行。

| 你在做什么 | 怎么调用 |
|---|---|
| 看页面正文或控件 | 运行 `aad browser read --url <页面地址>` 或加 `--view outline`。 |
| 用 WebKit 读正文或页面结构 | 启动本机 WebKitWebDriver 后运行 `aad browser read --webkit-port 4444 --url <完整 HTTP(S) 地址> --view outline`，正文用 `--view text`。每次调用打开一个临时浏览器会话，读取指定地址后关闭。已在 Linux WebKitGTK 验证。macOS Safari **[待验证]**。 |
| 同时取单号和提交状态 | 用 `aad browser read --url <页面地址> --fields '[{"as":"order","locator":{"name":"订单号"},"property":"value"},{"as":"saved","locator":{"name":"已保存"},"property":"present"}]'`。两个值来自同一次观察，在返回的 `values` 里。 |
| 看当前页的几列 | 在浏览器脚本中写 `page.table({role:"table", name:"Bills"}, ["ID","Amount"])`。 |
| 在已有脚本里收完整张表，再用结果继续操作 | 写 `const bills = page.collect(spec)`，检查 `bills.complete` 后遍历 `bills.records`。 |
| 在 YAML 或 MCP 里收整表 | Recipe 步骤使用 `collect_table`。MCP 调用 `control_collect_table`，传入同一组字段及页面的 `resource_id`。 |
| 临时导出 CSV，或和本地账单对账 | 在终端运行 `aad browser collect-table`，同时传 CSV 文件路径。 |

下面是一份采集参数。脚本可以直接传这个对象，使用 CLI 时把它存为 `bills.json`。`table` 指定哪张表，`columns` 列出要读的表头，`row_key` 选其中一个跨页唯一的业务列，例如账单号，`traversal` 指定如何翻页或滚动。示例中的 `expected_rows: 2` 表示账单应有两行。知道总数可以传入它；有首页按钮的分页表也可以通过观察首页和末页确认完整性。

```json
{
  "table": {"role": "table", "name": "Bills"},
  "columns": ["ID", "Amount", "Note"],
  "row_key": "ID",
  "traversal": {"mode": "pagination", "next": {"role": "button", "name": "Next"}},
  "expected_rows": 2
}
```

在同一次调用中收集数据、生成实际账单 CSV，并对比预期账单。URL 要与一个已打开的主页面完全一致。同 URL 的多个标签页或子 frame，可用 `--resource-id` 指定 `control_targets` 返回的资源。

```powershell
aad browser collect-table --url 'http://localhost:8080/bills' --browser-port 9222 --allow-input --spec ./bills.json --output ./actual.csv --compare-csv ./expected.csv --decimal Amount
```

`--output` 和 `--compare-csv` 均可省略。标准输出包含 `records`、`row_count`、`complete`、`stop_reason` 和实际提交的 `actions` 数量。对账时另有 `matches` 和 `comparison`。收集不完整会返回已有记录并以 1 退出，CSV 导出和对账都要求完整结果。完整但对账有差异时也以 1 退出，已请求的实际账单 CSV 仍会生成。

## 翻页与虚拟滚动

若分页器有“首页”按钮，可在 `traversal` 中加入 `"first": {"role": "button", "name": "First"}`。已禁用的 First 是首页证据；启用时，收集器对新观察中的 First 点击一次，等行数据更新、First 禁用后，从首页向后收集。`scope` 此时为 `pagination_from_first_page`。First 和 Next 的定位器要在按钮启用及禁用时均能识别它们。

仅给出 Next 时，收集器从**当前页**开始，`scope` 为 `pagination_from_current_page`。到达禁用的 Next 仅证明到达末页。若没有首页证据，完整对账还需要业务总数 `expected_rows` 与收集数相符，否则返回 `start_unverified` 或 `row_count_mismatch`。Next 消失时也需要匹配的业务总数。分页表的 `aria-rowcount` 可能只统计当前页，因此分页模式只接受调用者提供的业务总数。每次点击都通过新的观察定位控件并走普通动作的权限和身份检查。

虚拟表格需要从容器顶部开始。把 spec 的 `traversal` 换成下面的对象即可。在每次滚动后读取当前渲染的行，相邻窗口允许重叠。达到底部时还会核对唯一行数。总数来自调用者的 `expected_rows` 或表格的 `aria-rowcount`，后者包含一行表头。缺少总数时返回 `total_unverified`；数量不符返回 `row_count_mismatch`。

```json
{"mode":"virtual_scroll","container":{"role":"region","name":"Billing viewport"}}
```

输出按稳定键首次出现的顺序排列。同一窗口出现空键或重复键会报 `CONTROL.TABLE_KEY`。同一键再次出现时，各选中列必须与首次读到的值相同。运行期间应保持筛选、排序和数据集稳定，收集结束后浏览器停留在最后访问的位置。

| 范围 | 行为 |
|---|---|
| HTML 表格和 ARIA 网格 | 一次选 1–32 列。单行表头传列名，多层表头传完整路径，例如 `/2026/实付`。列名对应多个位置时会报错。支持的合并单元格、行列索引与限制见[多层表头说明](structured-table-headers.md)。 |
| 单次读取 | 观察最多 2,000 个节点。无法证明定位唯一时拒绝操作。每个窗口最多读取 200 行（含表头）、50 列，每格最多 500 个 UTF-16 单元，窗口字符预算为 200,000。达到裁切条件时返回不完整结果或表头错误。 |
| 整次收集 | `max_pages` 默认 64、最多 256，包含初始窗口。`max_rows` 默认及上限为 10,000。记录的 JSON 字节预算为 2 MiB。达到上限时检查 `page_limit`、`row_limit` 或 `output_byte_limit`。 |
| 动态更新 | 点击或滚动后最多等待约 2 秒获取更新。无进展返回 `no_progress`，不会自动再次点击同一页。CLI 收集请求的 `--timeout-ms` 默认及上限为 30,000。超时、取消或失效操作可能以错误结束。 |

## CSV 文件与差异

也可先保存完整的收集结果，再通过本地命令处理。两个命令的 `--input` 都读取包含 `columns`、`row_key`、`row_count`、`complete` 和 `records` 的收集对象。

```powershell
aad browser collect-table --url 'http://localhost:8080/bills' --browser-port 9222 --allow-input --spec ./bills.json > ./collection.json
aad csv export --input ./collection.json --output ./actual.csv
aad csv compare --input ./collection.json --csv ./expected.csv --decimal Amount
```

导出使用 UTF-8、CRLF 记录分隔符和 spec 中的列顺序。逗号、双引号和多行文本使用 CSV 引号转义。CSV 要求所选单元格都有字符串值，空字符串可往返，缺失单元格的 JSON `null` 会报错。输入必须是至多 16 MiB 的普通文件；导出在目标目录写临时文件，完整写入后发布到一个新路径。

对账按行键匹配，CSV 可以调整行、列顺序。文本字段逐值比较。可重复使用 `--decimal` 指定金额等十进制列；普通带符号十进制数最多支持 18 位小数，按精确值比较并汇总，超出整数运算范围会报错。带金额符号、千分位或指数记法的字符串需要先转换为普通十进制格式。结果包含 `missing`、`unexpected` 或逐单元格的 `changed`，按键排列。`difference_count` 是差异总数，最多展开 100 条，`totals` 分别列出实际和预期合计。

## MCP 和工作流

MCP 调用 `control_collect_table` 时，把 spec 的字段和 `resource_id` 放在同一参数对象中，服务端启动时授予 `--allow-input`。只读当前渲染窗口可使用 `control_read_table`。浏览器脚本可调用 `page.collect(spec)`。文件路径仅由本地 CLI 接收。

在已有工作流的 `requires` 中声明 `control.session` 能力和 `desktop.input`、`desktop.observe` 权限。下例将整个收集结果命名为 `ledger`，后续步骤可检查 `steps.collect.output.complete` 再使用记录。

```yaml
steps:
  - id: collect
    type: action
    uses: control.session.collect_table@1
    effect: {class: non_idempotent}
    risk: {category: modify, level: high}
    with:
      app: {provider: browser.cdp, url: 'http://localhost:8080/bills'}
      table: {role: table, name: Bills}
      columns: [ID, Amount, Note]
      row_key: ID
      traversal: {mode: pagination, next: {role: button, name: Next}}
      expected_rows: 2
outputs:
  ledger:
    value: "${{ steps.collect.output }}"
```
