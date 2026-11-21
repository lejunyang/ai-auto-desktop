# 网站导出账单：按任务下载并验收

`aad browser download-export` 处理**已经创建**的导出任务。调用方提供任务列表 URL、从网站得到的任务号、下载按钮和预期文件名。AAD 等待页面上这一项就绪，只点一次，再验收当前 Chromium 会话实际保存的文件。不需要知道网站的创建任务、轮询或签名下载接口。

例如，网站显示两行任务，下载链接都叫“下载”。目标行的无障碍名称是 `JOB-204`，该行显示日期“2026-11-19”和状态“生成完成”。将以下内容保存为 `bill-task.json`，把示例字段替换成该网站可观察到的精确名称：

```json
{
  "task_id": "JOB-204",
  "trigger": {"role": "link", "name": "下载"},
  "required_names": ["2026-11-19", "生成完成"],
  "expected": {"filename": "bill.csv"}
}
```

```powershell
aad browser download-export --url 'https://billing.example/exports' --spec ./bill-task.json --allow-input --headless --profile-dir ./billing-work --timeout-ms 900000
```

- 默认使用新启动的 Chrome，也可传 `--browser edge`、`--browser brave` 或 `--browser-executable`。省略 `--profile-dir` 会使用空的临时配置；需要登录时使用已经登录、未被占用的专用工作配置，不要指定日常浏览目录。CLI 只操作新启动的标签页，其主文档须到达传入的精确 URL（包括查询串和尾斜杠）。浏览器关闭后下载文件仍在本机。
- 默认搜索名称等于 `task_id` 的唯一 `row`。任务卡片可另传 `"scope":{"role":"group","name":"最近生成的账单"}`；若容器名称不是任务号，容器内必须有一个精确以任务号命名的元素。不是搜索任意一段包含任务号的文本。
- `required_names` 可省略，最多 16 个，例如期间、账户、完成状态。每项必须是该容器或其中一个唯一元素的精确无障碍名称。按钮或链接必须在同一容器内、名称唯一、可用。页面若需人工刷新或先点“查询”，CLI 不会猜测该操作；可用下述脚本按网站实际流程发起查询。
- `expected.filename` 是网站建议的**文件名**，不是路径。可另传整数 `bytes` 和 64 位十六进制 `sha256`，来自独立清单或可信上游时才能核对预期内容。没有预期摘要时，AAD 仍核对来源、文件名、完成状态和实收字节数，计算摘要用于交接；这**不证明账单行、日期或金额在文件内部正确**。

## 在现有流程里调用

在启动时允许输入的 MCP 会话中用 `browser_open` 打开 Chromium，然后在同一会话用 `browser_script`。脚本可先创建导出、读取回执，再把真实任务号与业务条件传入。例如当网站有一个名为“新建任务号”的只读输入框，且 `inputs` 提供上例的其他字段时：

```javascript
const id = page.read({role: "textbox", name: "新建任务号"}, "value");
return page.downloadExport({...inputs, task_id: id});
```

可以分两次调用：先创建任务并保存 ID，数分钟后只调用 `page.downloadExport(spec)` 下载，不必重新创建任务。一次等待默认 30 秒。CLI 上例等 15 分钟，最长 30 分钟（`--timeout-ms 1800000`）；通用脚本的 `timeout_ms` 仍以 120 秒为限。任务尚未就绪时，用同一份 spec 再运行只会复查同一任务，不会提交新任务。只通过端口连接的个人浏览器（含普通 `aad browser script --browser-port`）不会被改写下载目录，验收在点击前报 `BROWSER.EXPORT_UNTRACKED`。不支持 Firefox、弹窗新标签下载或系统“另存为”对话框。

## 如何判断结果

CLI 成功退出并返回 `result.verified: true`、`task_id`、实际匹配的 `scope`/`matched_names`，以及 `result.file` 的 `guid`、`frame_id`、`filename`、`path`、`bytes`、`sha256` 和两项预期值校验标记。将 **`file.path`** 交给下一步，不要自行拼接建议文件名：AAD 将实际读取、计算摘要的字节写入独立的 `verified-<UUID>/export.csv`（扩展名随文件而定）。关闭临时浏览器、删除临时配置或再次下载同名文件不会删除或覆盖这份副本。副本仍在临时存储中，应在系统清理前将需长期保存的文件归档，并由调用方清理下载目录。

点击前确认浏览器无进行中的下载并记下序号，仅接收之后任务 frame 的新 GUID；保存副本后再查一次，拒绝其他 frame 或竞争下载、事件丢失、取消、名称或字节/摘要不符。只读取 Chromium 完成事件明确报告的会话专用目录直接子文件，不扫描用户“下载”目录，不猜测缺失路径，不跟随文件软链接。单文件限 1 字节至 256 MiB。请在同一浏览器中串行下载：页面任务条件、时序和文件证据不是对网站内部任务协议的证明。页面或文件本身不可信时，请提供独立的预期摘要。

CLI 失败时非零退出。脚本报告的 `completed_actions` 是本次脚本内已完成的动作数；独立 CLI 调用中若为 1，说明已经点过下载，但验收失败，不要将目录里看到的其他文件当成成功，也不要自动重建任务。
