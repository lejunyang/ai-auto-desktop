# 统一控制与远程会话

`aad-control/1` 为 computer-use 与 browser-use 提供同一组操作：
`capabilities → targets → observe → find → act`，另有 `wait`、`cancel` 和 `close`。
它不依赖模型厂商的控制 API。平台适配器继续执行自己的实时元素校验；不支持的动作明确拒绝。

## 支持的动作与入口

| provider | 类型化动作 | 实际语义 / 限制 |
| --- | --- | --- |
| `desktop.windows_uia` | focus, invoke, click, fill, type_text, key, scroll, hover, drag | UIA + 原生输入；click 带 button/count/fraction 时走原生手势；节点必须暴露对应能力 |
| `desktop.linux_atspi` | focus, invoke, click, fill, type_text, key, scroll, hover, drag | AT-SPI；指针/键盘经 X11 XTest helper，只支持 KDE/X11 会话；派发前核对焦点、PID 与落点，drag 起止两点都核对 |
| `desktop.macos_ax` | focus, invoke, click, fill, type_text, key, scroll, hover, drag | AX helper；需要同一登录用户的辅助功能权限；指针动作要求目标应用在前台并经 AX 命中检查，事件只投递给目标进程；key 名称按字面（`ctrl` 是 Control，`cmd` 是 Command） |
| `browser.cdp` | focus, click, fill, key, scroll, hover, select, check, navigate, back, forward, reload, open_tab, activate_page, close_page, accept, dismiss, upload | 普通 click 为 `HTMLElement.click()` 的 DOM 激活；带 button/count/fraction 的 click、scroll、hover 为主 frame 的可信 CDP Input，落点先在页面里命中检查；fill/select 为 DOM 值及 input/change 事件；check 已是目标状态时不点击；key 为主 frame 的 CDP Input；navigate/back/forward/reload 与标签页动作 open_tab/activate_page/close_page 在 body 节点上，只限主 frame；accept/dismiss 只在 JavaScript 对话框节点上；upload 只在文件框上，路径受 `--allow-upload-dir` 限制；没有 drag |

以 `capabilities` 和观察节点的 `capabilities` 为准。可列出 provider 不表示当前 OS 已授予
桌面权限。启动失败或不可用原因在 `unavailable` 中返回。浏览器 key 只支持列出的导航/编辑键
和有限编辑组合，普通文本使用 fill。子 frame 不宣传 key、scroll、hover 和页面动作；OOPIF 等不支持路径明确报错。
DOM click/fill 的事件 `isTrusted=false`，并不等价于原生指针或真实逐键输入；需要 `isTrusted=true` 的页面，
给 click 一个 `fraction`（例如 0.5/0.5）走可信指针。可信指针先把元素滚进视口，再用 `elementFromPoint`
确认落点命中该元素，盖着别的元素时返回 `CONTROL.OCCLUDED` 且不发送任何输入。`navigate` 只接受 http/https，
不能打开 `file:`、`javascript:` 等地址。

标签页与弹窗：`open_tab`（要 `url`，同样只接受 http/https）在浏览器里新开一个标签页，答复里带新页面的 `page_id`；
`activate_page` 把被观察的页面切到前台，`close_page` 关闭它。三者都先在观察到的文档里确认 body 节点仍有效，
再经浏览器级连接（`/json/version` 给出的 `/devtools/browser/<id>`，固定连 127.0.0.1）发送 `Target.createTarget`、
`Target.activateTarget`、`Target.closeTarget`。页面自己打开的标签页或弹窗（`window.open`、`target=_blank`、
OAuth 登录弹窗）在 `targets` 里与其他页面一样列出，`metadata.opener_page_id` 取自 `Target.getTargets` 的
`openerId`，指向打开它的页面；不是被打开的页面该字段为 null。动作可能新开或关掉页面后要重新 `targets`。
弹窗通常需要用户手势，DOM 激活（`isTrusted=false`）的 click 会被弹窗拦截，用带 `fraction` 的可信 click。
实测（Windows，本机 Chrome headless，`browser_e2e` 的 `tabs_open_front_and_close_and_popups_name_their_opener`）：
open_tab 后新页 `visibilityState` 为 visible、原页 hidden，activate_page 后原页回到 visible，close_page 后
`targets` 不再列出该页，可信 click 触发的 `window.open` 弹窗带正确的 `opener_page_id`。

JavaScript 对话框（alert/confirm/prompt/beforeunload）：会话连接每个页面时先开 `Page` 域，只缓冲
`Page.javascriptDialogOpening/Closed` 两种事件。对话框打开后页面渲染进程被阻塞：`targets` 仍列出该页
（`metadata.dialog` 给出类型与文字，能力只有 accept/dismiss），`observe` 只返回一个 `role: alertdialog` 的节点，
`name` 是对话框文字，prompt 的 `value` 是默认答案，`dialog` 字段含 `type`/`message`/`default_prompt`/`url`；
在它上面 `accept`（prompt 可带 `text`，其他类型带 `text` 返回 `CONTROL.INVALID_REQUEST`）或 `dismiss`，
经 `Page.handleJavaScriptDialog` 处理；对话框已换成另一个或已关闭时返回 `CONTROL.STALE_TARGET`。
同一页面上的其他 act 与 capture 立即返回 `CONTROL.DIALOG_OPEN`（`not_committed`，`details.dialog` 带类型与文字），
不会等到超时。动作本身弹出对话框时（例如 click 的处理函数里调用 `prompt`），等待中的 CDP 命令被对话框事件
中断，动作按已派发返回，`backend_result.dialog_opened: true`；可信指针的 mouseReleased、按键的 keyUp 在这种情况下不再发送。
限制：会话连上页面之前就已打开的对话框无法经 CDP 处理——浏览器只允许看到它打开的那个连接应答，
被阻塞的页面也不响应新连接的 `Page.enable`。这种页面 2 秒内判定为无响应，`targets` 里标 `metadata.responsive: false`、
不提供能力，`observe` 返回 `BROWSER.PAGE_UNRESPONSIVE`，而不是让整个列表挂到超时；需要人手关闭对话框。
会话在 `targets` 时连接全部页面，所以之后打开的对话框都能处理；一个新弹窗如果在下一次 `targets` 之前就弹出对话框，属于上述限制。
实测（Windows，本机 Chrome headless，`dialogs_block_the_page_until_accepted_or_dismissed`）：prompt 带 text 接受后
页面读到答案，confirm dismiss 得 false，计时器弹出的 alert 使 fill 在 3 秒内被拒且未写入，会话前已打开的 alert 被标为无响应。
[待验证] Edge 与有界面（非 headless）浏览器的对话框行为；beforeunload 对话框只按同一事件路径处理，没有专门的 fixture。

文件上传：`<input type=file>` 只提供 `upload`（要 `files`，1..32 个绝对路径），不提供 fill/click，不会弹出文件选择框；
被页面用 `display:none` 藏在样式按钮后面的文件框也会出现在观察里（`states.hidden: true`），节点的 `files` 列出已选文件名。
路径由会话在服务端授权下解析：服务启动时用 `--allow-upload-dir <DIR>`（可重复，`aad mcp` 与 `aad control serve` 都有，
`--connect` 远程时只能在远端配置）给出允许的目录，没给时一律 `POLICY.DENIED`；每个文件必须存在且是普通文件，
按解析后的真实路径（符号链接、`..` 都先展开）判断是否落在允许目录下，否则 `POLICY.DENIED`，相对路径 `CONTROL.INVALID_REQUEST`，
不存在 `CONTROL.FILE_NOT_FOUND`；单选框给多个文件 `CONTROL.INVALID_REQUEST`。`capabilities` 的 `upload_dirs` 列出允许目录。
浏览器侧先在隔离 world 里核对节点，再取该元素的对象引用调用 `DOM.setFileInputFiles`，页面收到 input/change 事件。
实测（Windows，本机 Chrome headless，`uploads_reach_file_inputs_only_from_allowed_directories`）：允许目录内的文件上传后
页面读到文件名和大小，隐藏的多选文件框收到两个文件；目录外、相对路径、不存在、单选多文件和未授权会话均被拒且页面无变化。

动作参数：`click` 可带 `button`（left/right/middle）、`count`（1/2）、`fraction`；`type_text` 可带
`mode`（unicode/keys）；`scroll` 要 `direction`，可带 `notches`（1..30）、`fraction`；`hover` 可带
`ms`（1..5000，默认 500）、`fraction`；`drag` 要 `to`（同一次观察里另一个节点的 target），可带
`button`、`fraction`、`to_fraction`；`select` 要 `option`（选项文字或值，观察节点的 `options` 列出文字）；
`check` 要 `checked`；`navigate` 与 `open_tab` 要 `url`；`accept` 可带 `text`（≤4096）。`fraction` 是元素框内 0..1 的比例，不是屏幕坐标。

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

`wait` 由会话用 observe 与 find 反复实现，不依赖 provider，所以桌面与浏览器都有：给 `resource_id`、`locator` 和
`condition`（`appears` 恰好一个匹配；`disappears` 没有匹配；`state` 恰好一个匹配且其 `states` 等于给出的
`states` 对象），每隔 `interval_ms`（50..5000，默认 250）观察一次，直到满足或 `timeout_ms` 用完
（`CONTROL.WAIT_TIMEOUT`，`details` 带 `polls` 与最后一次是否存在）。多个匹配永远不算找到：一直歧义到超时返回
`CONTROL.AMBIGUOUS`。满足时返回最后一次的 `observation_id`，`appears`/`state` 还带该节点的 `target`，这次观察仍是当前观察，
可以直接 act。等待期间占用会话串行锁（单请求上限 30 秒），`cancel` 可以中止，结果为 `not_committed`。

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
aad mcp --allow-input --allow-profile-copy  # browser_open 可以复制已有 profile 的登录
aad mcp --allow-input --allow-upload-dir D:\to-upload  # 浏览器 upload 只能读这个目录下的文件
```

会话总带一个浏览器 provider，没连接时没有页面；`Session::attach_browser(port)` 之后换成那个浏览器
（旧的浏览器观察作废）。MCP 的 `browser_open` 用 `aad_cdp::launch` 启动浏览器后就这样接上，
`browser_record_*` 复用 `aad_cdp::recording` 写到存储目录的 `browser/<时间>-<id>/`。
`Capture { target }` 是只读操作：浏览器把元素滚进视口后用 `Page.captureScreenshot` 按元素框裁剪
（最大 4096×8192，超出时 `clipped: true`），会话加上 PNG 的 sha256 作为 `digest`。只支持主框架；
其他 provider 返回 `CONTROL.ACTION_UNSUPPORTED`。

MCP 工具为 `control_capabilities`, `control_targets`, `control_observe`, `control_find`,
`control_wait`, `control_act`, `control_cancel`, `control_close`，本机未限定授权时另有 `probe_environment` 与四个工作流工具。
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
MCP `save_workflow` 直接收这种步骤（`id` 可省，按 s1、s2 编号），拒绝 locator 里的 `node_id`；
带 `input` 的步骤会丢掉动作里的文字。工作流和同名录制文档一起写入，`describe_workflow` 逐步列出应用、
定位、动作和要传入的 input。旧的 Windows 动作格式（`set_value`、`window` 等）不再接受，
已保存的旧工作流仍可运行。实测（Windows）：`tests/windows/verify_mcp_saved_steps.py` 经 MCP 保存
fill 与 check，清空后 `run_workflow` 回放，文字由 WM_GETTEXT 独立读回。

## 录制

能录制的 provider 提供 `watch` / `collect` / `release`，`aad_control::recording::Recording` 统一驱动，
GUI 和 `aad record` 都用它。Windows driver 返回自己的录制步骤；AT-SPI 和 AX 返回同一种原始事件
（click、wheel、key、text、toggle，元素按各自快照的方式描述），由 `recording::from_events` 转成步骤：
连续文本合并为一次 fill，500 ms 内同一元素的两次点击是双击，连续滚轮累计格数，
紧接着引起勾选或文本修改的那次左键点击并入该修改，按键落到最近获得焦点的元素上。

- Linux：AT-SPI 的 `object:text-changed`、`object:state-changed:checked/focused` 信号，加上 XTest helper 的
  `record-input`（XRecord，只报告落在目标进程窗口上的点击、滚轮和目标进程有焦点时的按键）。
  点击发生后才按坐标查元素，界面在点击时立即变化（例如对话框关闭）会让该点击无法识别。
- macOS：Swift helper 的 AXObserver（值变化、焦点）与只监听的 CGEvent tap（需要“输入监控”权限，
  没有时 `pointer_watch: false`，只记录值变化）。

[待验证] Linux 录制只在 CI 的 Qt fixture 里检查点击和填写；macOS 录制的 Swift 代码未编译。
停止录制后，Linux 的 AT-SPI 信号线程要等到下一个信号才退出。

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
未在真实设备验证。Wayland 原生输入、完整 OOPIF、浏览器拖拽、结果缓存、
自动重放与统一 Android wire adapter 均不在本次范围。录制/回放及事件观察保持已有实现，不提升其可靠性声明。
