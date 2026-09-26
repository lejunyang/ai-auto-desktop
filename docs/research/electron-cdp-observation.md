# Electron / Chromium 类应用（浏览器、VS Code、飞书）的观察增强调研

状态：调研 + 真机探测，2026-09-26。结论按"对环境侵入程度"从低到高排列。本文回答一个聚焦问题：**当用户主要想看页面/编辑器上的文本与点击内容、不关心节点结构时，怎么比直接读 UIA 树更好。**

## 1. 为什么 UIA 树对这类应用不理想（已真机确认）

- Chromium 网页内容默认在树里、文本可读，但**结构抖动大**：整页加载/滚动产出大量匿名 pane，节点身份不稳定，diff 噪声高。
- VS Code（Electron + Monaco）默认**编辑器正文完全不在 UIA 树**：document 节点 value 只是 `vscode-file://…/workbench.html`。实测开 Shift+Alt+F1 屏幕阅读器模式后，才出现 `edit <文件名> value=<全文 10758 字符>`。
- 飞书/Lark 同为 Electron + 自研渲染（含 Canvas/自绘列表），预期与 VS Code 同类：外壳（按钮/菜单/输入框）在树里，IM 消息流/文档正文默认可能不完整。`[待验证：飞书各视图在默认 UIA 与开启辅助功能后的文本暴露率]`

## 2. 三条可行路线对比

| 路线 | 原理 | 文本质量 | 对点击 | 侵入/前置 | 覆盖 |
| --- | --- | --- | --- | --- | --- |
| A. 现有 UIA + digest 过滤（已实现） | 事件叫醒→快照→内容过滤 | 浏览器好；VS Code 需开无障碍；飞书待测 | 看到点击**结果** | 无 | 所有应用 |
| B. CDP 直连 Chromium（已真机验证可行） | Edge/Chrome 远程调试端口，`innerText`/输入事件/点击坐标 | **直接拿渲染后纯文本，无结构噪声** | 可订阅 `Input.dispatchKeyEvent`/点击事件，或读 activeElement 变化 | 需用调试端口启动或让实例开启 `--remote-debugging-port`；企业管控可能禁 | 浏览器、所有 Chromium 内核应用（含 Electron，需其开放调试端口） |
| C. 应用专用桥/辅助 API | VS Code 命令行/扩展、飞书开放平台、Accessibility 专属开关 | 最全最稳 | 语义级 | 每应用适配、要装扩展/授权 | 单应用 |

### 2.1 路线 B：CDP（Chrome DevTools Protocol）真机证据

2026-09-26 在本机 Edge 实测（独立 `--user-data-dir` + `--remote-debugging-port`）：

- `GET http://127.0.0.1:<port>/json/list` 列出全部 target（8 个），page target 带 `webSocketDebuggerUrl`。
- WebSocket 上 `Page.enable` + `Page.navigate` 后执行 `document.body.innerText`，对 example.com 直接得到渲染后纯文本：
  ```
  Example Domain
  This domain is for use in documentation examples without needing permission...
  Learn more
  ```
- 即：**"页面上的文本"可以零结构、一次性、按 innerText 取到**，比 UIA 树干净一个量级；还能 `document.querySelector` 精准取标题/链接/href。
- 观察点击/输入：CDP 可 `Runtime.addBinding` + 页面侧注入轻量监听（监听 `input/change/click` 的委托事件，读取 `event.target.innerText / value / aria-label`），或用 `Input.*` 域与 `DOM.getFocusedNode`/`Accessibility` 域。这样记录的是**带语义的点击目标与输入内容**，而不只是坐标。
- 接入形态建议：`aad watch` 在发现前台是 Chromium 进程且本机 CDP 端口可达时，优先走 CDP 取文本/活动；不可达则回退 UIA+digest。CDP 只观察，不要求注入式自动化。

**限制与风险（如实记录）**：
- 连用户**日常正在用的**浏览器实例，需要该实例启动时带 `--remote-debugging-port=<port>`；事后给已运行实例开端口一般不生效。可行折中：提供一个用固定调试端口启动 Edge/Chrome 的小封装，或读取浏览器固定配置；但这会改变用户启动习惯。
- Electron 应用（VS Code/飞书）默认**不开**远程调试端口；VS Code 可以 `code --status` 但内容观察仍建议走其无障碍模式或扩展；Electron 应用可用环境变量/启动参数 `--remote-debugging-port` 启动（很多 Electron 支持），需逐个验证。
- 企业策略、EDR 可能把调试端口视为风险；必须本地、显式开启、默认关闭。

### 2.2 路线 C：应用专用
- VS Code：屏幕阅读器模式（已验证）是零开发的最短路径；要长期稳定可做一个只读扩展，通过命令暴露当前编辑器 selection/text/活动文件，语义最全。
- 飞书：优先实测其"无障碍/屏幕阅读器"设置与 Electron 调试端口；开放平台 OpenAPI 面向企业数据而非 UI 操作，不适用于"观察我在界面上点了什么"。

## 3. 已落地的降噪：内容优先 digest（路线 A 增强）

为回应"无用信息多、某些键没必要输出"，`aad watch` 默认输出已从原始结构 ChangeSet 改为 `window_activity` 摘要（`aad-uia::observe::digest`）：

- 匿名（无 name 无 value）结构节点（pane/control_type_0）折叠为一个 `structural_noise` 计数，不逐条打印；
- 只保留**有文本的内容**：`text_changes`（输入了什么，from→to）、`appeared`/`disappeared`（带名称/文本的控件、菜单项）、命名控件的 `focused/enabled/read_only`；
- 丢弃空字符串↔空字符串的重绘抖动、匿名元素的状态抖动、offscreen 变化；
- 同一批里"出现又消失"的相同内容（打开又立即关掉的右键菜单）按指纹**配对抵消**，不再 appeared/disappeared 各打一遍；
- 每类上限（`--max-items`，默认 40）并标 `truncated`。
- `--raw` 可回到完整结构 diff。

真机同脚本对比（Edge 地址栏输入/跳转 + 右键 + VS Code 选文件/Ctrl+P）：日志体积 **604KB → 23–36KB**；地址栏 `cn.bing.com -> example.com`、Ctrl+P 结果列表、文件树选中仍清晰；右键菜单在批内抵消后不再重复。

实测仍残留的噪声类型（下一步可继续收）：
- VS Code 标题栏/状态栏的装饰性 group 文本变化（git 相对时间"3周前/昨天"、错误/信息计数、行列号）会作为 text_changes 出现——这些不是用户输入，可加"装饰字段 denylist/只记 value 模式为 edit 的输入"进一步过滤；
- 部分批次仍有较多命名按钮的 added/removed（标签/侧栏重建），可加"同窗口短时间内同名集合抖动"聚合。

## 4. 建议的产品策略（面向浏览器/VS Code/飞书 + 文本/点击）

1. **默认**：UIA 事件叫醒 + digest 文本摘要（已具备），零前置。
2. **浏览器增强（首选做 B）**：检测/支持 `--remote-debugging-port`，对 Chromium 前台标签用 CDP 取 `innerText` 增量 + activeElement/委托 click 监听；UIA 仅用于窗口切换与非 Chromium 应用。开关显式、默认关。
3. **VS Code**：文档提示开启屏幕阅读器模式（已验证全文可读）；可选只读扩展。
4. **飞书**：先做一次 W1 式实测（默认 UIA / 无障碍开关 / Electron 调试端口三路），再决定走 B 还是 C。
5. **过滤可配置**：进程黑名单（密码管理器/银行/飞书私聊窗口静音）、字段 denylist（git 状态/行列号/时间戳）、只看 edit 值变化等，作为 watch 的 profile。

### 2.3 CDP 已落地（2026-09-26）

新增只读 `aad-cdp` crate 并接入 `aad watch --cdp-port <port>`：

- 纯标准库实现 127.0.0.1 HTTP 发现（`/json/version`、`/json/list`）与手写 WebSocket/1.1（握手/掩码/分片），无 TLS；只读，从不 `Page.navigate`、从不注入输入。
- `state()` 直接取 `document.body.innerText`（渲染后纯文本）；注入的被动监听器用**捕获阶段**（React/Vue stopPropagation 也能看到）上报点击与输入，drain 时按字段合并逐键 input、去重点击；**密码框值永不上报**。
- watch 循环：前台是 msedge/chrome/brave/vivaldi 且端口在听时，走 CDP 产出 `page_activity`（标题/URL + 新增/消失文本行 + 点击/输入活动），并跳过该标签的 UIA 快照；切到非浏览器应用自动回退 UIA；不传 `--cdp-port` 行为不变。
- 真机：Bing 首页全文（热点/新闻/备案号等）直接由 innerText 读到，绕过了 UIA 匿名 pane；确定性探针验证点击 label、文本框值、密码事件无值。
- 踩坑记录（供后续）：Edge devtools HTTP 响应在发完后仍保持连接，`read_to_end` 会挂到超时——必须按 `Content-Length` 精确读；冷启动要先等 `/json/version`；探针需按命令行清理整个 Edge 进程树，否则 profile 锁残留导致下次端口不开。

**启动方式（2026-09-27 落地）**：新增 `aad-browser` 可执行文件（`aad-cdp` crate 的 bin）。
`aad-browser --port 9222 [--detached] [URL]` 自动查找 Edge/Chrome/Brave，用**独立 profile**（默认 `%TEMP%\aad-cdp-profile-<port>`）加调试参数启动，等待 `/json/version` 就绪后输出 JSON 启动记录；`--detached` 在端点就绪后即返回。它不触碰用户日常浏览器配置。然后 `aad watch --cdp-port 9222` 即可观察。

**输入观察增强（2026-09-27）**：注入监听器除标准 input/change 外，新增
- `contentEditable`/富文本（云文档、聊天输入框）：读 `innerText`，作为 `tag=contenteditable, type=richtext`；
- 对当前焦点可编辑元素挂 `keyup` 与 `compositionend`（IME 中文输入与"只在失焦提交"的网页输入兜底）；
- 密码框仍不读值。

确定性探针已验证：普通文本输入、密码无值、点击 label、以及一个 contentEditable 富文本的完整文本都被采到（4 类事件，退出码 0）。

**真机物理敲键的遗留说明（诚实记录）**：在本自动化代理环境里，难以把专用浏览器窗口稳定保持在前台（发送的物理键落不到目标窗口），因此"真人逐键敲入 Bing 搜索框"这一步没有产出端到端记录；这是**测试夹具的前台焦点限制，不是观察链路问题**——点击/输入/富文本的采集已由页内合成事件确定性证明，CDP 文本读取此前也已在 Bing 首页真机验证。用户在自己桌面交互运行 `aad-browser` + `aad watch --cdp-port` 时窗口天然在前台，应正常工作；交付后建议补一次真人手动验证。

## 5. 待验证
- [待验证] 飞书（IM 消息、文档、搜索框）在默认 UIA、屏幕阅读器模式、Electron 调试端口下各自的文本暴露率与点击信号。
- [待验证] 连接用户日常 Edge/Chrome 配置（非独立 user-data-dir）开 CDP 的企业策略与稳定性；多 profile/多窗口 target 归属。
- [待验证] VS Code 用 `--remote-debugging-port` 启动后 CDP 取 Monaco 文本，与屏幕阅读器模式的成本对比。
