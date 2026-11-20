# 录一次、写得短、跑完能对账

## 已经能做的事

| 你要做什么 | 入口 | 收到什么 |
|---|---|---|
| 探索界面、查几个按钮 | `control_observe`、`control_find_many` | 一次观察中的元素，可继续选一个操作。 |
| 取订单号、实付额等字段 | `control_read_fields` 或步骤里的 `read` | 一次返回命名值；`expect` 可把实付与前一步的报价作比较。 |
| 读网页表格里的几列 | `control_read_table({target, columns:["订单号","金额"]})` | 先观察、找到 Chromium HTML 表格，再传入它的 target；返回每行一个对象、当前渲染的数据行数、截断原因。隐藏或缺失的格子为 null，不会错位。单行表头中请求的列必须唯一，合并单元格会报错。 |
| 少和模型来回说话 | 一次性 1–32 步用 `run_steps`；重复任务 `save_workflow` 后按名运行 | 逐步选择窗口、观察和操作都在执行器内。返回结果、步骤耗时；控制步骤失败时有最后成功、失败、下一个未开始的步骤 ID。 |
| 审阅和修改长流程 | `aad recipe orders.recording.json --out orders.yaml` | 同一应用只在阶段开头写一次，重复的定位器用别名，动作各占一行。导出时会读回 YAML，检查展开后的工作流是否与原始录制一致。已有输出文件不会被覆盖。 |
| 执行、定位断点 | `aad validate orders.yaml`，然后 `aad run`；传 `--journal`，运行中或事后可用 `aad review` | 查看最后完成的步骤、最后动作错误、已开始但未收到结果的动作。只读日志，不发送输入；日志没有结尾时会明确标出。 |
| 避免录错同名按钮 | 浏览器录制自动保留显式命名的行、表单等父容器；Linux、macOS 原生录制按捕获到的控件身份归并 | 两个订单行都叫“保存”，重排后按行名回放；没有可区分的标签时，定位有歧义就停。原生文本跨相邻轮询合并，焦点切换、跳过的事件或缓冲区溢出需要特别审阅。 |
| 给同事汇报录制情况 | `aad browser-record summary --out <录制目录>` | 给出是否正常结束、步骤数、缺口数、过滤的请求数。只读最终 manifest 白名单字段；不复制原始证据、网址、截图、备注、表单值或网络正文。原始录制仍单独保管。 |

## 写一个短流程

以下为可执行格式。分阶段写 `app`，步骤沿用原有动作、字段读取和断言。省略步骤 ID 时依次编号；跨步骤引用时明确命名。一个 Recipe 最多 500 步。也可用它表达浏览器和原生窗口交替的操作。

```yaml
kind: Recipe
name: submit-draft
targets:
  save: {role: button, name: 保存草稿}
phases:
  - app: {provider: browser.cdp, title: 报销}
    steps:
      - {id: save, locator: save, action: {type: click}}
      - id: receipt
        read:
          - {as: saved, locator: {name: 草稿已保存}, property: present}
        expect: {saved: true}
        within_ms: 3000
```

```powershell
$env:AAD_CDP_PORT = '9222'  # 本机测试浏览器实际开启的端口
aad validate ./draft.yaml
aad run ./draft.yaml --permission desktop.observe --permission desktop.input --journal ./draft-run-001.ndjson
aad review ./draft-run-001.ndjson
```

每次运行给日志取一个新名字。比如第 53 步是检查回执：如检查失败，先看业务订单，再决定是否继续。如果进程在提交期间被杀，`review` 给出 `unconfirmed_action` 和 `effect: unknown`。先用订单号查清楚，别直接从头再跑。有 `run.finished` 时查看最终状态；没有时 `no_final_event` 也可能是流程仍在运行。未写完的最后一行会单独标出；中间丢行、混入其他运行的日志会报错。

## 本机实测

计时覆盖本地测试应用的执行和结果核对，模型推理时间另计。浏览器使用独立的 headless Edge 配置，桌面使用隔离 X11 会话里的真实 Qt 应用。

| 流程 | 检查方法 | 结果 |
|---|---|---|
| 浏览器 100 步，填订单 → 提交 → 读回执 → 检查控件 | 从独立 CDP 连接读订单数组，按顺序逐笔比较 | 25 笔一致；录制 1,509 行、Recipe 106 行；一轮 1.43 秒。 |
| 同样流程 500 步 | 除独立读回外，也检查每笔回执出现在工作流 outputs 中 | 125 笔一致；7,509 行 → 506 行；一轮 7.84 秒。 |
| 100 步网页与桌面交替，网页生成回执 → 读号码 → 填入 Qt → 核对 | 浏览器独立连接核对提交数，Qt 应用从自身文本变更信号核对每次收到的号码和顺序 | 25 次逐笔相符，从 ORD-1 到 ORD-25；一轮 7.39 秒。 |
| 100 步中第 53 步故意期待错误的回执号 | 从日志读最后开始的步骤，从应用读已落地的订单 | 止于 s53，最后成功 s52，只有前 13 笔。 |
| 点击已生效、回执未返回时强杀执行器 | 测试页先写入订单再暂停；杀掉执行器、解除暂停、读取页面和运行日志 | 页面恰有一笔；`aad review` 报出尚未确认的 submit；复查日志后仍只有一笔。 |
| 两行都有“保存”，录制后交换行顺序 | 通过 CLI 导出并实际回放，从页面读按钮所属订单行 | 只点中原来的 Order A。 |
| 大表、隐藏行、截断以及重复表头 | 构造 51 列、长单元格、声明 1,000 行但只渲染 1 行；另测隐藏单元格和歧义列 | 返回行/列/单元格/字符预算的实际缺口，隐藏值不冒充另一列，重复列名明确报错。 |

复现入口见[浏览器长流程与崩溃注入](../tests/browser/long-workflow.test.mjs)、[网页与桌面交替回放](../tests/browser/mixed-workflow.test.mjs)、[表格及金额断言](../tests/browser/batch.test.mjs)、[行重排](../tests/browser/observer.test.mjs)、[分享摘要与断连](../tests/browser/recording.test.mjs)、[Linux 原生录制](../tests/linux/run-rust-atspi-fixture.sh)。

## 上次调研的缺口现在还剩什么

| 方向 | 已交付 | 仍需单独开发或验收 |
|---|---|---|
| 效率 | 同屏多查、字段投影、短批、按后端发现、逐步耗时和长流程实测 | 每个后端内部“枚举/快照/定位/输入”耗时拆分；真实远程延迟和多站点 P95，没有实测数据。 |
| 数据与结果 | 命名字段、跨步骤比较、轮询断言、按表头取列、截断报告 | 自动跨页取全量、虚拟滚动去重、列表映射作者语法、下载内容/CSV 哈希/网络完成条件。当前可人工写下翻页步骤，当前页面的记录不能当作所有页。 |
| 录制质量 | 控件身份归并、显式网页行/容器定位、缺失计数、只含计数的可分享摘要 | 没有可访问名称的自绘控件、原生同名行的稳定语义锚点、自动把浏览器/系统对话框/文件管理器串成一份录制。 |
| 审阅与维护 | 可逆短 YAML、按应用分阶段、运行日志、断点核对 | 可视化时间轴、图文证据逐步对齐、可复用子流程的版本和发布、从自然语言自动生成并修复定位策略。 |
| 恢复 | 已有持久只读运行的暂停/恢复；普通写流程可查日志并手工对账 | 业务幂等键、业务方对账接口和人工确认后从指定阶段续跑。日志本身无法告诉你服务器是否已经扣款。 |
| 浏览器覆盖 | Chromium 路径本轮真浏览器验证；已有 Firefox BiDi 主文档能力 | Firefox 子 frame 写入、WebKit；closed shadow / canvas 内部视觉定位；引擎差异和登录态迁移仍需目标环境验证。 |
| 原生系统 | Linux X11 真应用回读；Windows 目标交叉检查 | **[待验证]** Windows 交互桌面、多屏 DPI、中文 IME；macOS AX/TCC、Secure Input 和本轮 Swift 录制在 Mac 真机的行为。Wayland 仍需完成可操作后端并经过用户授权的 portal/输入链路验收。 |
| 部署与脚本 | 本地/SSH 控制、输入租约、工作流权限。MCP 可提交整批声明式步骤，受信任的本地工作流可用 `aad run --allow-scripts` 运行脚本 | 普通浏览器 MCP 没有任意页面 JS 或 Playwright 代码入口。本地脚本需要自己准备数据，执行器不会注入浏览器的 `page`。无人值守机器池、账号/会话调度、按次业务审批尚缺。 |

原始竞品依据见[23 项逐项对比](research/browser-computer-recording-efficiency-2026-11-21.md)；本页记录后续代码与验收进展。
