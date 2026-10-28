# 统一控制与远程会话

`aad-control/1` 为 computer-use 与 browser-use 提供同一组操作：
`capabilities → targets → observe → find → act`，另有 `cancel` 和 `close`。
它不依赖模型厂商的控制 API。平台适配器继续执行自己的实时元素校验；不支持的动作明确拒绝。

## 支持的动作与入口

| provider | 类型化动作 | 实际语义 / 限制 |
| --- | --- | --- |
| `desktop.windows_uia` | focus, invoke, click, fill, type_text, key, scroll, hover, drag | UIA + 原生输入；click 带 button/count/fraction 时走原生手势；节点必须暴露对应能力 |
| `desktop.linux_atspi` | focus, invoke, click, fill, type_text, key, scroll, hover, drag | AT-SPI；指针/键盘经 X11 XTest helper，只支持 KDE/X11 会话；派发前核对焦点、PID 与落点，drag 起止两点都核对 |
| `desktop.macos_ax` | focus, invoke, click, fill, type_text, key, scroll, hover, drag | AX helper；需要同一登录用户的辅助功能权限；指针动作要求目标应用在前台并经 AX 命中检查，事件只投递给目标进程；key 名称按字面（`ctrl` 是 Control，`cmd` 是 Command） |
| `browser.cdp` | focus, click, fill, key, scroll, hover, select, check, navigate, back, forward, reload | 普通 click 为 `HTMLElement.click()` 的 DOM 激活；带 button/count/fraction 的 click、scroll、hover 为主 frame 的可信 CDP Input，落点先在页面里命中检查；fill/select 为 DOM 值及 input/change 事件；check 已是目标状态时不点击；key 为主 frame 的 CDP Input；navigate/back/forward/reload 在 body 节点上，只限主 frame；没有 drag |

以 `capabilities` 和观察节点的 `capabilities` 为准。可列出 provider 不表示当前 OS 已授予
桌面权限。启动失败或不可用原因在 `unavailable` 中返回。浏览器 key 只支持列出的导航/编辑键
和有限编辑组合，普通文本使用 fill。子 frame 不宣传 key、scroll、hover 和页面动作；OOPIF 等不支持路径明确报错。
DOM click/fill 的事件 `isTrusted=false`，并不等价于原生指针或真实逐键输入；需要 `isTrusted=true` 的页面，
给 click 一个 `fraction`（例如 0.5/0.5）走可信指针。可信指针先把元素滚进视口，再用 `elementFromPoint`
确认落点命中该元素，盖着别的元素时返回 `CONTROL.OCCLUDED` 且不发送任何输入。`navigate` 只接受 http/https，
不能打开 `file:`、`javascript:` 等地址；新开标签页尚未支持。

动作参数：`click` 可带 `button`（left/right/middle）、`count`（1/2）、`fraction`；`type_text` 可带
`mode`（unicode/keys）；`scroll` 要 `direction`，可带 `notches`（1..30）、`fraction`；`hover` 可带
`ms`（1..5000，默认 500）、`fraction`；`drag` 要 `to`（同一次观察里另一个节点的 target），可带
`button`、`fraction`、`to_fraction`；`select` 要 `option`（选项文字或值，观察节点的 `options` 列出文字）；
`check` 要 `checked`；`navigate` 要 `url`。`fraction` 是元素框内 0..1 的比例，不是屏幕坐标。

## 观察视图与定位器

`observe` 总是保留完整捕获（`max_nodes` 上限 2000），`view` 只决定返回什么，不影响哪些节点可以 find/act：

| `view.mode` | 返回 | 适用 |
| --- | --- | --- |
| `nodes`（协议默认） | 全部节点 | 需要原始树的程序 |
| `outline`（MCP 默认） | 可操作或有名字、不在屏幕外的节点，按 `limit`（默认 80，≤500）与 `max_characters`（默认 20000）截断；回报 `matched`、`shown`、`stopped_by` | AI 阅读 |
| `overview` | 区域及其大小，不列内容 | 大窗口先看结构；只有 Windows 计算区域，其他平台返回 `CONTROL.VIEW_UNSUPPORTED` |

`view.region` 只用于 outline，取 overview 报出的区域名，与定位器 `within` 用的名字一致。
Windows 的 outline 和 overview 直接复用本次观察的 driver 快照，不会另拍一张。

`find` 的定位器：`role`、`name`、`node_id` 在所有 provider 上精确匹配；`value`、`automation_id`、
`class_name`、`framework_id`、`states`、`match`（exact/contains）、`within`、`near`、`nth` 由 Windows
driver 求值，其他 provider 返回 `CONTROL.LOCATOR_UNSUPPORTED`，不会悄悄忽略。匹配多个时返回
`CONTROL.AMBIGUOUS`（Windows 为 `DRIVER.AMBIGUOUS_MATCH`）并在 `details.candidates` 列出候选；
`expect: optional` 时找不到返回 `found: false` 而不是错误，多个匹配仍然报错。

给人用的 `aad apps/describe/overview/snapshot/find/do` 也走这个会话：每条命令一个进程、一个会话，
`aad do` 在同一个进程里观察、定位、操作，接受与 `aad find` 相同的定位参数，不跨进程传引用。

CLI 的持久协议入口在三平台均可用：

```powershell
aad control serve                       # 一个只读会话，stdin/stdout NDJSON
aad control serve --allow-input          # 由可信操作者明确授予输入
aad mcp                                  # 本机 MCP，默认只读
aad mcp --allow-input                    # 本机 MCP，授予输入与工作流写入/运行
aad mcp --provider desktop.linux_atspi    # 限定 provider，不提供工作流工具
aad mcp --browser-port 9222              # 显式连接本机已有的隔离 CDP 实例
```

MCP 工具为 `control_capabilities`, `control_targets`, `control_observe`, `control_find`,
`control_act`, `control_cancel`, `control_close`，本机未限定授权时另有 `probe_environment` 与四个工作流工具。
每次调用可带 `request_id` 与 `timeout_ms`。`control_cancel` 的被取消请求字段名为 `target_request_id`；
MCP cancellation notification 也能取消仍在执行的统一操作。错误响应带 `hint`，说明可行的恢复方式。
原先按动作拆开的 Windows MCP 工具（`list_apps`、`describe_window`、`find_element`、`invoke` 等）已移除，
能力并入上述工具；`run_workflow` 与 `control_act` 共用会话的 driver、串行锁和输入租约。
远程 MCP 或指定 provider/resource 时只暴露统一工具，避免绕过授权。

## 引用、期限与结果

直接 NDJSON 示例（每行一个对象；先发送第一行，读取真实 session ID 后再发送后续行）：

```json
{"request_id":"c1","operation":{"method":"capabilities"}}
{"request_id":"t1","session_id":"<returned-session-id>","operation":{"method":"targets"}}
{"request_id":"o1","session_id":"<returned-session-id>","operation":{"method":"observe","resource_id":"<returned-resource-id>","max_nodes":1000,"view":{"mode":"outline"}}}
{"request_id":"f1","session_id":"<returned-session-id>","operation":{"method":"find","observation_id":"<returned-observation-id>","locator":{"name":"Save","role":"button"}}}
{"request_id":"a1","session_id":"<returned-session-id>","timeout_ms":3000,"operation":{"method":"act","target":{"session_id":"<returned-session-id>","resource_id":"<returned-resource-id>","observation_id":"<returned-observation-id>","node_id":"<returned-node-id>"},"action":{"type":"click"}}}
```

`target` 绑定会话、资源、观察及节点。跨会话引用、篡改资源、重复 ID、未知字段和裸坐标均拒绝。
每个 provider 仅保留最新一次观察，最长 5 分钟；新的观察尝试或派发的输入尝试会撤销旧引用。
截断观察不能证明唯一性，不能用于 find/act。动作前还要实时校验原生元素，或浏览器的
page/frame/loader/document、节点连接状态及语义指纹。导航、frame 替换和节点替换使引用失效。
动作成功后应重新 observe，并验证实际效果；成功派发不能证明业务目标已达成。

响应同时包含兼容的 `effect` 和跨项目语义字段 `commit_state`：

| commit_state | 含义 |
| --- | --- |
| `not_committed` | 输入未派发，例如授权、过期、取消在提交边界前胜出 |
| `committed` | 适配器返回已派发；仍需观察业务效果 |
| `unknown` | 输入可能已派发，但取消、超时、断连或验证失败导致最终结果未知 |

读取成功的 commit_state 为 not_committed。输入提交前后有原子取消边界；底层 OS 调用不能强制撤销。
截止时间覆盖排队及传输，单请求上限 30 秒。原生系统调用可能不可中断；此时客户端按时返回 unknown，
服务端仍持有输入租约直到调用结束，避免下一会话抢占。取消/关闭不等待普通请求的串行 gate。

会话上限 1 小时、10,000 个已见请求 ID；耗尽后撤销会话。仅保留有界去重集合，不缓存可重发的动作结果。
最多 8 个请求在途，请求 64 KiB、响应 8 MiB；空闲连接 60 秒关闭，客户端每 20 秒只读保活。
关闭、EOF、断连和到期撤销观察与输入权限。重新连接创建全新会话，必须重新发现和观察；
**不会恢复输入、自动重放或自动重试 unknown 动作**。

## 远程授权与隔离

服务没有 TCP/HTTP 监听。认证由创建 stdio 的 OS/SSH 层完成；每个连接启动独立进程、独立会话，
服务端启动参数不可由协议请求扩大：

```powershell
# 已有 SSH 身份、主机密钥校验和目标机 aad；以下 host/account 是示例占位符。
aad mcp --connect '["ssh","-T","-o","BatchMode=yes","-o","StrictHostKeyChecking=yes","automation@example-host","aad control serve --allow-input --provider desktop.linux_atspi"]'

# 同一远程连接也可向本地 NDJSON 客户端提供协议代理。
aad control connect --command '["ssh","-T","-o","BatchMode=yes","-o","StrictHostKeyChecking=yes","automation@example-host","aad control serve"]'
```

本机 executable 由 argv 直接启动，不经本机 shell；SSH 的最后一项仍按 SSH 远端命令约定执行，
因此必须是可信操作者固定的命令，不能拼接模型或页面文本。不要把这个 stdio parser 直接接到未经认证的 socket。
`--provider` 与 `--resource` 可重复指定，只有服务端拥有授权；先用只读连接发现资源，再配置固定授权。
资源 ID 来自当前平台对象身份，并不是重启后长期稳定的设备标识。

首次输入时，同一 OS 用户下协作会话通过文件锁获取独占 `desktop-input` 租约，涵盖 native 与 browser，
到会话关闭或输入调用结束才释放。Unix 锁目录检查 owner/0700、拒绝符号链接；锁文件无凭证内容。
可信管理员可用 `AAD_CONTROL_LEASE_DIR` 配置所有相关进程共享的私有目录。
该租约只能约束使用本接口的进程，不能阻止人手输入或绕过接口的其他自动化软件。
桌面控制进程必须与目标 GUI、DBus/显示服务或 macOS TCC 授权位于正确用户环境；SSH 登录本身不保证这些条件。

CDP 仅接受 loopback 调试地址，显式 `--browser-port` 才启用；不会启动真实浏览器、复制登录态或创建凭证。
浏览器调试端口本身有高权限，使用专用空白 profile 并保留 Chromium 沙箱。每个会话使用独立 isolated world，
关闭时清理自己的 DOM 引用，不影响其他只读观察者。

## 与 Android 的互操作边界

两个项目对齐 session/resource/observation/target、类型化能力和 `not_committed|committed|unknown`。
这一版本没有实现跨项目 wire adapter：Android 仍需明确 serial、同用户 bridge/token store/ADB server、
独占 device lease，视觉观察按其当前规则撤销，WebView 不等于这里的通用 CDP DOM。
上层可统一编排生命周期和不确定结果处理，不能假定两个 provider 的全部动作、观察租期或引用格式可直接互换。

## 保存的步骤

`aad_control::steps` 定义平台无关的步骤：`app`（provider、进程或应用名、标题片段、网址前缀，
必须恰好匹配一个目标）、`locator`、`action`（不含会话引用；drag 的终点写在 `to`）或 `check`
（`present` 与 `within_ms`），以及可选的 `input`（fill/type_text 的文字运行时传入，敏感值不落盘）。
回放时列目标 → 选出唯一的应用 → observe → find → act，每次都重新定位。

录制文档是 `kind: Recording`、`format: control-steps/1`；编译出的工作流只用
`control.session.act@1` / `check@1`，声明 `desktop.input`、`desktop.observe` 权限。
`aad run` 和 MCP `run_workflow` 都注册这个 provider。只含步骤的工作流不在会话串行锁下运行
（每个请求自己取锁），把步骤和直接 driver 动作混在一起的工作流会被 MCP 拒绝。
Windows 的 UIA 录制结果由 `steps::from_windows_capture` 转成这种格式，无法回放的动作返回原因。

## 验证与剩余范围

`osdk run control-test` 跑纯逻辑、真实子进程 stdio、三平台原生适配 mock 和 MCP 回归。
Linux CI 另外运行带沙箱的空白 Chromium fixture，检查 DOM/可信键盘、可信指针（右键、双击、悬停、滚轮、被遮挡时拒绝）、
select/check、navigate/back/forward/reload 的实际结果及导航/frame/节点过期；同一个测试在 Windows 上用本机 Chrome 跑过
（`AAD_TEST_CHROME` 指向 chrome.exe，临时空白 profile；本机的 Edge 用同样参数启动后立即退出，原因未查）。
隔离 Qt/AT-SPI fixture 通过真实 CLI 子进程（`aad control serve`，以及 `aad apps`、`aad do`、`aad find`）检查 fill/invoke 后的界面状态。
Windows 上 `tests/windows/verify_*_effect.py` 用自建的隔离探针窗口，经 `aad do` 检查右键、中键、双击、两个方向的滚轮、悬停、拖拽、
按键输入和保存的工作流，由 Raw Input 日志独立确认。所有 GUI fixture 串行执行。
CI 和 release 工作流使用官方 one-sdk Action 最新正式版本，不依赖 pending one-sdk PR。

[待验证] 本云容器不能启动本机 Chromium 沙箱，浏览器实际效果由 GitHub Linux CI 验证。
[待验证] Linux 上新的 CLI 段、双击和 Ctrl+A 只在 CI 的 Xvfb 环境里跑，本机只编译了 helper 并跑了单元测试；
右键、滚轮、悬停、拖拽在 Linux 上还没有观察效果的 fixture。
[待验证] macOS 的 pointer_gesture、drag、key_chord：Swift 代码没有在本机编译过，Rust 侧只跑了单元测试。
在 Mac 上依次验证：`plugins/macos_ax/build.sh` 能编译签名；打开“文本编辑”，`aad apps` 取得它的 resource id 后，
`aad do key <id> --role AXTextArea --keys cmd,a`、`aad do type-text ... --text x` 替换全文；
`aad do click <id> --role AXTextArea --button right` 弹出菜单；`aad do scroll <id> --role AXTextArea --direction down`
在长文档里滚动；在“访达”列表里 `aad do drag` 把文件拖进文件夹。
Windows/macOS 的新适配路由在 CI 以 mock 编译/测试；Windows 的输入桌面、macOS TCC 和跨机器 SSH
未在真实设备验证。Wayland 原生输入、完整 OOPIF、浏览器拖拽与新标签页、Linux/macOS 的自动录制、结果缓存、
自动重放与统一 Android wire adapter 均不在本次范围。录制/回放及事件观察保持已有实现，不提升其可靠性声明。
