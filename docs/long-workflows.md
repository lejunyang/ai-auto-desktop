# 录一次、写得短、跑完能对账

## 已经能做的事

| 你要做什么 | 入口 | 收到什么 |
|---|---|---|
| 探索界面、查几个按钮 | `control_observe`、`control_find_many` | 一次观察中的元素，可继续选一个操作。 |
| 取订单号、实付额等字段 | `control_read_fields` 或步骤里的 `read` | 一次返回命名值；`expect` 可把实付与前一步的报价作比较。 |
| 读网页正文、页面结构或几个字段 | `aad browser read`，配合 `--view outline` 或 `--fields` | 在一次观察里读完。Chromium 默认读正文，Firefox 默认读页面结构。 |
| 读表格里的几列 | `control_read_table({target, columns:["订单号","金额"]})` | 传入已观察的表格，按行返回命名字段和截断情况。Chromium 支持多层表头，Linux 原生应用可读取通过 AT-SPI 暴露的表头和文本单元格。 |
| 少和模型来回说话 | 一次性 1–32 步用 `run_steps`；重复任务 `save_workflow` 后按名运行 | 逐步选择窗口、观察和操作都在执行器内。返回结果、步骤耗时；控制步骤失败时有最后成功、失败、下一个未开始的步骤 ID。 |
| 审阅和修改长流程 | `aad recipe orders.recording.json --out orders.yaml` | 同一应用只在阶段开头写一次，重复的定位器用别名，动作各占一行。导出时会读回 YAML，检查展开后的工作流是否与原始录制一致。已有输出文件不会被覆盖。 |
| 执行、定位断点 | `aad validate orders.yaml`，然后 `aad run`；传 `--journal`，运行中或事后可用 `aad review` | 查看最后完成的步骤、最后动作错误、已开始但未收到结果的动作。只读日志，不发送输入；日志没有结尾时会明确标出。 |
| 避免录错同名按钮 | 浏览器录制自动保留显式命名的行、表单等父容器；Linux、macOS 原生录制按捕获到的控件身份归并 | 两个订单行都叫“保存”，重排后按行名回放；没有可区分的标签时，定位有歧义就停。原生文本跨相邻轮询合并，焦点切换、跳过的事件或缓冲区溢出需要特别审阅。 |
| 给同事汇报录制情况 | `aad browser record summary --out <录制目录>` | 给出是否正常结束、步骤数、缺口数、过滤的请求数。只读最终 manifest 白名单字段；不复制原始证据、网址、截图、备注、表单值或网络正文。原始录制仍单独保管。 |

## 写一个短流程

需要循环、分支和可复用子流程时，使用 `format: recipe/2`。如果已有 JavaScript 数据处理逻辑，可直接用浏览器脚本中的 `page` 读取界面、操作控件和返回结构化结果。[Recipe 循环与子流程](recipe-programs.md) · [浏览器脚本 API](browser-script.md)

账单跨页时，`aad browser collect-table` 可将完整记录导出 CSV，按行键核对金额并列出差异。任务横跨浏览器和多个桌面应用时，用 `aad browser record mixed start` 选择网页和窗口，录下的 `workflow.json` 可直接回放。[表格采集与对账](table-collection.md) · [跨应用录制](mixed-recording.md)

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

想知道长任务慢在哪一步，查看 `aad review` 的 `slowest_steps`。它从这份运行日志挑出耗时最长的五次已结束步骤，给出步骤名、毫秒数、成功或失败状态和日志事件号。同一个步骤在循环里运行多次时，可按事件号找到那一次。

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
| 20 页、1,000 笔本地订单 | 从首页开始采集，与独立生成的 CSV 比较文件内容和金额合计 | 翻页 19 次，CSV 逐字节相同，双方金额合计 5,005.00。 |
| 同一次运行内跨 10 页收集 100 单，循环调用固定版本的提交子流程 | 在子流程里填单、提交、读回执，用独立 HTTP 请求读取测试服务实际接收的订单 | 服务端逐笔确认 100 张订单，回执顺序和值与输入相同。限制只收第一页时提交数为零。 |
| 网页、两个桌面应用和弹窗交替输入 | 在独立 X11 会话录制，重启两个应用，回放后核对两个进程的通知和网页的请求 | 六个操作按原顺序生效。同名的 Reference 输入框分别收到各自的值。 |

复现入口见[浏览器长流程与崩溃注入](../tests/browser/long-workflow.test.mjs)、[网页与桌面交替回放](../tests/browser/mixed-workflow.test.mjs)、[表格及金额断言](../tests/browser/batch.test.mjs)、[行重排](../tests/browser/observer.test.mjs)、[分享摘要与断连](../tests/browser/recording.test.mjs)、[Linux 原生录制](../tests/linux/run-rust-atspi-fixture.sh)。

## 上次调研的缺口现在还剩什么

| 方向 | 已交付 | 仍需单独开发或验收 |
|---|---|---|
| 效率 | 同屏多查、字段投影、按后端发现、批量操作。一次 `run_steps` 可以完成 10 次点击，逐条调用对照需要 30 次往返。短批结果按查窗口、观察、定位、执行、字段读取和表格采集分别汇总调用次数、失败次数和耗时。[结果字段](control-sessions.md) | 后端单次调用内的细分耗时、远程连接和多站点的 P95 延迟。 |
| 数据与结果 | 命名字段、分组表头和 ARIA 网格、跨页采集、按任务号验收导出文件、CSV 逐行对账、读取指定 XLSX 区域 | 直接对接业务系统的订单、回执和对账接口需要相应系统的适配。 |
| 录制质量 | 网页和指定桌面应用共用时间线。HTML 附件可在回放时更换。网页用 `showSaveFilePicker()` 成功选定新文件后，可自动生成带目的地参数的另存为步骤。[录制另存为](browser-recording.md#录下网页的另存为) | 录制目录选择、浏览器菜单保存网页、系统下载询问保存位置、识别自绘对象和稳定区分原生列表中的同名行。 |
| 审阅与维护 | 短 YAML、循环和分支、本地版本化子流程、运行日志；桌面编辑器可以对照录制截图和当前元素、修改定位并从选中步骤试跑 | 跨应用混合录制的完整图文时间轴、跨团队发布目录、自动提出定位修复建议。 |
| 恢复 | 持久只读运行可暂停/恢复。写流程可在开始时启用恢复日志，中断后凭业务回执继续 | 需要业务方提供可靠回执。跨运行的全局去重和跨账号恢复仍需业务系统配合。 |
| 浏览器覆盖 | Chromium 和 Firefox 脚本可逐层进入授权弹窗，再逐层返回。Firefox 同源子页面支持已定位控件上的按键、指针和滚轮。WebKitGTK 可以打开指定 URL 并读取正文、标题、链接和按钮。 | Firefox 跨域或变形子页面的物理输入，WebKit 的写操作和脚本。[Firefox 子页面条件](testing/firefox-frame-input.md) |
| 画布与自绘 | Chromium 能在已观察的 Canvas 上点击、悬停、滚动、拖动和画折线。自由布局可以读出可见文字及其位置，实测脚本能找到“REVIEW”并点击。规则网格可按行列读数。[画布操作与读取](testing/canvas-grid-ocr.md) | 识别无文字的图形、区分同名节点、读出连线两端、跨滚屏拼接整张画布，以及布局变化后修复录制时的落点。 |
| 原生系统 | Linux X11 和 KWin Wayland 已验证原生表单和文件窗口。Wayland 上加载输入插件的 Qt 5 Widgets 应用可对已确认的窗口和控件发送按键和指针动作。[Qt 应用启动与操作](../crates/aad-linux/qt-wayland-input/README.md) | Wayland 其他工具包的定向键鼠、跨窗口拖放。**[待验证]** 用户同意屏幕共享后的完整链路、Windows 和 Mac 的交互桌面、混合 DPI 多屏。 |
| 部署与脚本 | 本地/SSH 控制、输入租约、声明式步骤。CLI 和 MCP 都支持 JavaScript 通过 `page` 操作选定的标签页。工作流中的 Python 计算步骤由 `aad run --allow-scripts` 启用。 | 无人值守机器池、账号和会话调度、按次业务审批。 |

原始竞品依据见[23 项逐项对比](research/browser-computer-recording-efficiency-2026-11-21.md)；本页记录后续代码与验收进展。

### 按实际任务看剩余缺口

| 任务 | 现在能做什么 | 卡在哪里 |
|---|---|---|
| 浏览器已经弹出系统文件窗口 | `aad browser choose-files` 选择普通附件或原生文件句柄，`choose-directory` 选择已有目录，`save-as` 选择新文件路径。已在 X11 的 GTK portal 和 KWin Wayland 的 GTK、KDE portal 核对文件内容或返回的目录 URI。[平台条件与用法](browser-recording.md#在已经打开的文件选择器里选文件) | Windows 标准对话框和 macOS AppKit 面板已接入，交互桌面 **[待验证]**。原生目录句柄需要单标签、单 frame 的明确发起者。 |
| 从后台导出昨天的账单，交给下一步处理 | 在页面创建任务并读任务号，按号等待完成，下载并验证到手的文件。CSV 可以和预期账单逐笔比日期、金额，再把文件路径交给下游。[导出账单](browser-download-export.md) | 每个网站需要提供任务行、就绪状态和下载控件的定位。 |
| 第 80 单提交后断网，第二天接着处理 | 从第一次运行用 `--recovery-log`，中断后[按业务键或显式回执续跑](workflow-recovery.md)，不再派发已确认动作 | 需要业务端独立回执；普通 NDJSON 日志只能复查，查不到订单不能当作未提交。 |
| 从复杂报表或本地工作簿取数 | Chromium 可读多层表头、语义网格，以及给出行列布局的可见等宽等高画布网格。后者逐格返回 OCR 文本和置信度。CSV 导出可读已保存的 XLSX，也可指定运行中的 Windows Excel 窗口、工作表和区域。Linux 可读运行中的 GTK 表格。[画布网格](testing/canvas-grid-ocr.md) · [XLSX 区域](workbook-csv.md) · [Excel 未保存数据](live-excel-csv.md) · [GTK](linux-atspi-table.md) | 自由布局画布、需要滚动取全的纯画布和低置信度单元格仍需人工判断。**[待验证]** Windows 桌面 Excel 的实际导出，以及 Qt 和 LibreOffice Calc 的原生表格。 |
| 网站改版后维护一段录制 | 在桌面编辑器导入浏览器录制目录，对照旧页面截图和当前元素，替换后直接试跑剩余步骤。[修复步骤](browser-recording.md#网站改版后修复一个步骤) | 当前录制需要有该步截图才能显示旧画面。编辑器由操作者挑选替代控件。 |
| 在新页完成授权后回主站 | Chromium 和 Firefox 脚本可以从业务页进入授权页、再进入身份验证页，最后逐层返回，在原业务页读取回执。[跨页脚本](browser-script.md) | 需要浏览器能确认弹窗的来源，孤立的标签页需要单独指定资源。 |
