# 统一控制与远程会话

`aad-control/1` 为 computer-use 与 browser-use 提供同一组操作：
`capabilities → targets → observe → find → act`，另有 `wait`、`cancel` 和 `close`。
它不依赖模型厂商的控制 API。平台适配器继续执行自己的实时元素校验；不支持的动作明确拒绝。

## 支持的动作与入口

| provider | 类型化动作 | 实际语义 / 限制 |
| --- | --- | --- |
| `desktop.windows_uia` | focus, invoke, click, fill, type_text, key, scroll, hover, drag, paste, minimize, maximize, restore, close_window, activate | UIA + 原生输入；click 带 button/count/fraction 时走原生手势；节点必须暴露对应能力。指针动作先对落点做 UIA 命中检查，键盘动作先证明窗口在前台、焦点在元素内（见下文）。另有只读的 capture / read_text，以及需单独授权的 launch、clipboard_read、global_key |
| `desktop.linux_atspi` | focus, invoke, click, fill, type_text, key, scroll, hover, drag, paste, minimize, maximize, restore, close_window, activate | AT-SPI；指针/键盘经 X11 XTest helper，本机 KDE/X11 已验收，GNOME/XFCE X11 待真机验收，Wayland 输入尚未实现；派发前核对焦点、PID 与落点，drag 起止两点都核对；窗口动作只在顶层窗口节点上、需要 EWMH 窗口管理器。另有只读 capture / read_text（Tesseract），以及需单独授权的 launch、clipboard_read、global_key（见下文“其他平台”） |
| `desktop.macos_ax` | focus, invoke, click, fill, type_text, key, scroll, hover, drag, paste, minimize, maximize, restore, close_window, activate | AX helper；需要同一登录用户的辅助功能权限；指针动作要求目标应用在前台并经 AX 命中检查，事件只投递给目标进程；key 名称按字面（`ctrl` 是 Control，`cmd` 是 Command）；窗口动作只在 AXWindow 节点上，maximize 即全屏。另有 capture（需屏幕录制权限）、read_text、launch、clipboard_read、global_key。[待验证] 全部新增项需 macOS 真机 |
| `browser.cdp` | focus, click, fill, type_text, key, scroll, hover, drag, select, check, navigate, back, forward, reload, open_tab, activate_page, close_page, accept, dismiss, upload, emulate, clear_site_data | 普通 click 为 `HTMLElement.click()` 的 DOM 激活；带 button/count/fraction 的 click、scroll、hover、drag 为可信 CDP Input（任何 frame，含跨进程 OOPIF），落点先在页面里命中检查；fill/select 为 DOM 值及 input/change 事件；check 已是目标状态时不点击；type_text/key 为发给焦点元素的可信 CDP 键盘输入；navigate/back/forward/reload、标签页动作 open_tab/activate_page/close_page 以及 emulate/clear_site_data 在 body 节点上，只限主 frame；accept/dismiss 只在 JavaScript 对话框节点上；upload 只在文件框上，路径受 `--allow-upload-dir` 限制 |
| `browser.firefox_bidi` | focus, invoke, fill, click, hover, scroll, key, type_text, select, check, navigate, back, forward, reload, open_tab, activate_page, close_page | Firefox WebDriver BiDi；主文档写操作，子 frame 只读；click/hover/scroll/key/keys 文本使用可信输入，invoke/fill/check/select 使用 DOM 语义。key/单击支持有界长按。支持安全局部 capture 和锚定 body 的导航/标签页操作；历史只支持可证明目标的连续同源 HTTP(S) 片段。双击、指针修饰键尚未实现，参数范围见下文 Firefox 专节 |

以 `capabilities` 和观察节点的 `capabilities` 为准。可列出 provider 不表示当前 OS 已授予
桌面权限。启动失败或不可用原因在 `unavailable` 中返回。

原生 provider 与 `browser.cdp` 的 `key`、单次 `click` 和 `drag` 接受可选 `hold_ms`，范围 1–5000 毫秒。
按键在按下组合键后等待，点击在按下按钮后等待，拖拽在起点按下后等待再移动；
每次调用均负责释放自己按下的键和按钮，不允许跨调用遗留按下状态。
`click` 带 `hold_ms` 时使用真实指针路径，不能同时指定双击；全局快捷键不支持长按。
请求的剩余时间不足时，在按下前拒绝；等待中继续检查焦点、命中和截止时间，
失败后尝试释放并如实报告输入可能已生效。旧版 Linux helper 未声明 `bounded-hold` 时拒绝长按。
会话取消通过显式 token 传到支持取消的原生 provider；取消信号先于可能阻塞的操作锁发布。
Windows 的独立 watchdog 检查取消并释放本次持有的输入，后续派发也检查同一 token；
文本、粘贴、全局键及窗口动作在派发与等待边界检查取消。同步 UIA 调用本身仍可能阻塞，
所以及时释放输入不代表该调用立即返回。macOS 写 RPC 在取消后终止 helper 并等待清理：
通常最多 500 ms，粘贴最多 2500 ms，以容纳已在执行的 AX 调用及剪贴板恢复；之后必要时强制回收。
macOS helper 被取消后不能复用，需重建原生会话/provider。截止时间和长按上限始终保留。
未实现 cooperative hook 的第三方 provider 只在调用前检查取消；已提交的系统效果不能撤回，
取消后的结果可能为 `unknown`。上述 Windows/macOS 原生释放效果 **[待真机验收]**；
宿主模拟 provider、真实子进程清理测试和交叉检查只证明相应的软件边界。
本机 Qt 独立事件计时确认请求 200 ms 时按键 205 ms、点击 201 ms、拖拽首次移动 205 ms；
按键验收使用 F2，不代表可打印字符经过 IME 的长按行为已验收。Windows/macOS 长按 **[待真机验收]**。

AT-SPI 中心点命中校验无法确认落在新鲜目标子树时返回 `DRIVER.POINT_NOT_PROVEN`，
`effect: not_applied`，不发送输入。重新观察并定位具体叶子节点，或滚动后重试；目标支持键盘动作时也可使用键盘。
该错误表示命中证明失败；元素缺少动作能力仍返回 `DRIVER.ACTION_UNSUPPORTED`。

Chromium/CDP 键盘：先聚焦被观察元素并确认焦点（含开放 shadow root 内的深层焦点）和文档焦点都在它上面，再发键。
`type_text` 只在可编辑节点上提供，逐字符发送，每个字符前重新确认焦点，焦点移走即以 `CONTROL.FOCUS_LOST`
（`details.typed` 为已发字符数）停止：`mode: unicode`（默认）每字符一次 `Input.insertText`（有 input 事件，
无按键事件）；`mode: keys` 发 keydown/keypress/keyup（带 key/code，`isTrusted=true`），只接受 US 布局可打出的字符，
其他字符在发送前整体拒绝（`CONTROL.INVALID_REQUEST`，`details.character`）。`key` 接受针对焦点元素的编辑/导航键：
Enter、Tab、Escape、方向键、Home/End、PageUp/PageDown、Backspace/Delete、单个字符、F2、Ctrl 或
Ctrl+Shift 加字母，以及 Shift 或 Ctrl+Shift 加方向/Home/End。Ctrl+K/S 等可用于网页编辑器。
这些按键通过 renderer 路径派发，不触发浏览器快捷键的预处理或未处理回退；因此 Ctrl+T/W 也仅是
页面事件，标签页操作仍用专门的 open_tab/close_page 等动作。Ctrl+Tab/PageUp/PageDown、
Alt/Meta 及 F2 以外的功能键仍以 `CONTROL.ACTION_UNSUPPORTED` 拒绝。
按下或释放返回错误时，以独立的短清理预算尝试 keyUp，结果通过 `key_released` 标明。
2026-11-07 本机有界面 Chromium 对有/无页面处理器共 18 组 Ctrl 组合完成真实检查：
页面收到可信事件，原生编辑器焦点、窗口/对话框集合、网址和标签页不变；另有超时后释放验收。

Chromium/CDP drag：在可拖起的元素（`draggable`、列表/网格项、grab/move 光标）及已确认的 Canvas/closed-shadow surface 上提供，终点可以是同一观察里的任何节点。
两端都滚进视口后在当前位置各自命中检查（不再滚动），被遮挡或无法同时可见则 `CONTROL.OCCLUDED` 且什么都不按下；
随后按下、12 步滑动、松开。HTML5 拖放经 `Input.setInterceptDrags` 截获，再用 `Input.dispatchDragEvent`
（dragEnter/dragOver/drop，带拖拽数据）在终点完成。实测 headless Chrome 只用鼠标事件也能完成 HTML5 drop，
但有界面的 Windows 浏览器会进入系统拖放循环，所以总是截获 `[待验证: 有界面 Chrome/Edge on Windows]`。

`drag.path` 可指定最多 32 个中间锚点，每项为 `target` 与元素内的 `fraction`；起终点也必须显式给出比例，
所有锚点必须引用同一观察中的同一 surface。浏览器仅支持 Canvas 与已确认的 closed-shadow host，
按起点、中间点、终点依次派发可信鼠标事件，每段至少 16 ms；`hold_ms` 仍是起点按下后的停留时间。
按下前复核全部锚点，段间继续检查身份、布局和命中；预算不足、遮挡或变化时拒绝或停止并释放。
自定义路径不支持 HTML5 draggable；异常时释放鼠标、取消拖放并关闭截获，清理结果在错误详情中报告。
省略 path 保持原来的两端拖动。Chromium 已用公开截图 PNG 独立核对 Canvas/closed-shadow 内的折线，
并回读可信事件顺序及超时、取消、遮挡和几何变化后的释放；不代表跨页面或跨资源拖拽已支持。

Linux/X11 后端在 helper 声明 `anchored-drag-between` 时支持两个资源之间的普通 `drag`：
先分别观察两端，再将源 target 与目标 `to` 提交；需要两端各自的授权、有效观察与输入租约。
第一版仅接受不含自定义路径的直线，窗口须同时可见，途中其他应用/弹窗会使动作拒绝或停止并释放。
两端在每阶段重新校验；本机 Qt 两进程的实际数据传递已验收，约需 4.8 秒。
具体 4096 像素路线、窗口装饰与观察保留边界见 [Linux 驱动](architecture/linux-kde-x11-driver.md)。
Windows/macOS 与浏览器跨资源拖拽仍未启用。

Shadow DOM：观察深度优先进入每个开放 shadow root（先宿主的 shadow 树，再其 light 子节点——只有被 slot 的才会渲染），
内部控件与普通节点一样有指纹、复核和动作；命中检查经 `shadowRoot.elementFromPoint` 穿过宿主，包含关系沿 host 向上。
closed shadow 的内部节点仍不出现在公共观察里，但后端通过 CDP 私下确认 root 后，可将已观察的 host 标为
`surface: closed_shadow`，提供可信点击、hover、scroll 与拖拽；使用 host 内的比例位置并重新核对命中。
默认 click 也走 CDP 指针事件，使内部控件收到真实点击。截图可以提供其视觉内容，text 视图不展开内部文字。
每次观察、动作和截图会检查内部受保护字段；密码/hidden 字段只记录身份，不导出其值。
已记录的字段即使改为普通文本并移出 shadow，仍保持保护；相关受保护内容可见时拒绝截图。
私有 CDP DOM 响应可能含 HTML 属性，但不会导出给调用者，也不用于读取字段值。
检查超过预算或不能证明子 frame 安全时，拒绝相应能力/截图；整页预算不足不影响可完整检查的普通局部观察。
本机 Chromium 已验收点击后的像素变化、trusted 事件、局部拖拽/滚动、遮挡与过期目标拒绝，以及两个密码字段
同时 reveal/移到 light DOM 后的持续保护。未知 closed shadow 不会自动获得 surface 权限。

Frame：页面连接用 `Target.setAutoAttach`（flatten）跟随跨站 OOPIF，`control_targets` 把每个 frame 列成资源，
metadata 带 `parent_frame_id`、`out_of_process`；跨站 iframe 一开始是进程内的 about:blank，换进程后资源 id 不变。
每个 frame 在其自己的 CDP session 里建隔离 world 观察，DOM 动作就在那里执行。可信指针一律发到页面自己的
session、用主 frame 坐标：先把各层 frame 宿主元素滚进视口，用其 content box 得出偏移，再在每一层用
`DOM.getNodeForLocation`（文档坐标，需要加上该层滚动量）确认命中的是目标 frame 或下一层宿主，页面盖在 frame 上的元素
因此以 `CONTROL.OCCLUDED` 拒绝；在 frame 内还会反复移动指针，直到 frame 自己报告目标 `:hover` 才按键。
实测：OOPIF 刚滚进视口后立刻派发的输入落在主 frame 上，要等合成器跟上（约 300 ms），所以用 `:hover` 确认；
直接发到 OOPIF 自己 session（frame 坐标）的鼠标事件也能送达，但绕过页面的命中检查，因此不用。键盘发到页面 session，
由浏览器送到焦点 frame。截图 `control_capture` 仍只限主 frame。
DOM click/fill 的事件 `isTrusted=false`，并不等价于原生指针或真实逐键输入；需要 `isTrusted=true` 的页面，
给 click 一个 `fraction`（例如 0.5/0.5）走可信指针。可信指针先把元素滚进视口，再用 `elementFromPoint`
确认落点命中该元素，盖着别的元素时返回 `CONTROL.OCCLUDED` 且不发送任何输入。
`navigate` 默认只接受 http/https；操作者可在服务器启动时显式授予本地文件目录或浏览器内部来源：

```powershell
aad mcp --browser-port 9222 --allow-input `
  --allow-browser-file-dir /absolute/path/to/test-pages `
  --allow-browser-internal-origin chrome://settings
```

两个授权参数均可重复。目录必须已存在；只接受其内可访问的普通文件，按 canonical 路径检查，
拒绝目录索引、远程 file 主机和越界符号链接。内部来源必须是明确的 `chrome://host`、`edge://host`
或 `chrome-extension://id`，不支持通配符；来源内路径、查询和 fragment 可以变化。
`javascript:`、`data:`、`blob:` 不能作为 navigate/open_tab 目标。当前已存在的普通 data 文档仍可观察。
未授权页面在 targets 中保留诊断、动作能力为空；观察/操作前复核当前文档及其祖先 frame 的授权，
截图还检查可能包含的子 frame。授权不保证浏览器允许该特权页面使用 CDP 隔离 world，后端拒绝时不会改走任意脚本。
长按、连续输入、拖拽各段与 HTML5 drop 前也会重新检查当前文档和祖先的 URL、loader 与嵌入关系，
文件授权按当时 canonical 文件复核；失效后不再派发后续输入，只尝试释放自有键鼠和取消拖放。
取消和截止时间在 fresh 查询前后检查，准备阶段的滚动/聚焦也受同一边界约束；正在等待的 CDP 命令仍受其截止时间约束。
`back`/`forward` 在导航前检查历史目标的文档授权，保留已有空白页、data 文档和长 http(s) URL 的历史访问。
这是本项目的文档访问门禁，不是浏览器文件系统或网络沙箱：页面脚本、资源加载仍遵循浏览器自身权限。
本机已验收本地 HTML 的实际点击与越界拒绝、`chrome://version` 观察，以及 `chrome://settings` 打开搜索框并输入后回读。
本机 Chromium 自身拒绝 file 页跨文件路径的 `pushState`；fragment 更新正常。另以授权文件消失且 DOM/loader 不变的方式，
实测 8 条输入链及 srcdoc 祖先授权失效后的停止/释放，不把它描述成已复现跨路径 pushState 绕过。

标签页与弹窗：`open_tab`（要 `url`，使用与 navigate 相同的服务器授权）在浏览器里新开一个标签页，答复里带新页面的 `page_id`；
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
中断，动作按已派发返回，`backend_result.dialog_opened: true`；键鼠释放使用独立的短清理预算尝试派发，
清理结果不能替代页面业务效果的回读。
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

下载：浏览器级连接建立时（第一次 `targets` 或标签页动作）用 `Browser.setDownloadBehavior`（`allow`，`eventsEnabled`）
把下载放进本会话专用的临时目录 `aad-downloads-<uuid>`，并只缓冲 `Browser.downloadWillBegin/downloadProgress` 事件。
MCP `browser_storage`（只读）列出某个页面或 frame 资源的 cookie（name、domain、path、expires、session、http_only、secure、same_site、size）和其 origin 的 localStorage 键及值长度；cookie 值只在服务端以 `--allow-cookie-values` 启动时给出（`control_capabilities` 的 `cookie_values`），storage 值从不给出。
MCP `browser_network`（只读）列出页面自本会话连接以来最近的请求（每页最多 500 条，`dropped` 计遗忘数）：method、url（去掉凭据、fragment 和敏感查询值）、类型、状态（pending/finished/failed/redirected）、HTTP 状态、MIME、耗时与大小；`include_bodies` 时完成的文本响应带最多 4096 字符正文，JSON 敏感字段打码。只看页面 session 的 Network，OOPIF 内部发出的请求不在其中（限制）。

MCP `browser_downloads`（只读，`--allow-input` 不需要）列出 `directory`、`tracking`（是否正在把下载放进该目录，否则原因在 `reason`）
和每个下载的 `guid`、`url`、`suggested_filename`、`state`（in_progress/completed/canceled）、`received_bytes`、`total_bytes`、
`path`（浏览器报告的 `filePath`，没有时取目录下的建议文件名）；最多记住最近 200 个。文件在会话结束后保留。
只对本服务启动的浏览器（`browser_open`）这样做：经 `--browser-port` 等方式连上的用户自己的浏览器不调用
`setDownloadBehavior`，下载设置保持原样，`browser_downloads` 回答 `tracking: false`、
`reason: "attached browser: downloads are not redirected"`。`browser_close`、再次 `browser_open` 和会话关闭时，
先尽力把启动的浏览器恢复为 `behavior: "default"`，之后 `tracking: false` 并说明已交还。
实测 Chromium 在任何浏览器级 DevTools 客户端断开时都会清掉这个设置（连已断开的那个客户端从没设置过也一样），
所以对启动的浏览器，会话在每次用浏览器级连接和每次 act 之前重新设置一次。
实测（Windows，本机 Chrome headless，`downloads_land_in_the_session_directory_and_are_listed`）：点击带
`Content-Disposition: attachment` 的链接后列表出现 completed 的 `report.txt`，路径在会话目录下，内容与服务端一致。
[待验证] 有界面浏览器与 Edge 的下载行为；恢复为 default 后浏览器的实际下载位置未单独验证（测试只核对会话不再路由）。

动作参数：`click` 可带 `button`（left/right/middle）、`count`（1/2）、`fraction`；`type_text` 可带
`mode`（unicode/keys）；`scroll` 要 `direction`，可带 `notches`（1..30）、`fraction`；`hover` 可带
`ms`（1..5000，默认 500）、`fraction`；`drag` 要 `to`（同一次观察中的目标节点，可为同一 surface），可带
`button`、`fraction`、`to_fraction`；`select` 要 `option`（选项文字或值，观察节点的 `options` 列出文字）；
`check` 要 `checked`；`navigate` 与 `open_tab` 要 `url`；`emulate` 要 `preset`（desktop/tablet/mobile/reset）或 `color_scheme`（light/dark/no_preference）或两者，只有预设名、没有尺寸或位置字段，效果持续到本会话与该页的连接断开；`clear_site_data` 用 `Storage.clearDataForOrigin` 清掉页面 origin 的 cookie、localStorage、IndexedDB、缓存与 service worker（不含 sessionStorage）；`accept` 可带 `text`（≤4096）；`paste` 要 `text`（1..4096）。`fraction` 是元素框内 0..1 的比例，不是屏幕坐标。
`capabilities` 每个 provider 另列 `reads`（`capture`、`read_text`），并只在授权时列出 `launch`（允许启动的程序）、
`clipboard_read: true`、`global_keys: true`。

## Firefox WebDriver BiDi（首版）

Firefox 使用独立的 `browser.firefox_bidi` provider。连接操作者已启动的本机 BiDi 端口，
不复用 Chromium/CDP 命令，也不在失败时切换协议。CLI 和 MCP 入口分别为：

```powershell
aad control serve --allow-input --provider browser.firefox_bidi --browser-bidi-port 9223
aad mcp --allow-input --provider browser.firefox_bidi --browser-bidi-port 9223
```

`--browser-bidi-port` 与 `--browser-port` 互斥，只连接 loopback `/session`。
一个 provider 持有一条连接和一个 Firefox 自动化 session；已有自动化 session 占用时明确失败。
关闭控制会话会结束自己的 BiDi session，不关闭浏览器。当前包括 headless 在内的输入均保守占用
`desktop-input`，不会仅凭网页或 user agent 声称 headless 就免除桌面互斥。

观察、find、wait、HTML id scope 和从已观察元素派生的 `scope_target` 可用。
观察基于有界 DOM 语义与独立 sandbox realm，不能视为 Firefox 原生无障碍树；
身份 token、节点指纹、当前 realm、文档及祖先 URL 授权在操作前重新检查。
子 frame 可单独观察，但动作能力为空，首版不向子 frame 写入。

主文档支持 DOM `focus`、`fill`、`invoke`、`check`、`select`。fill 是替换值并派发 input/change，
invoke 是 DOM 激活；这些不承诺可信用户事件。`click` 始终使用可信指针，支持左/中/右键及元素内比例，
只接受单击；`hover`、`scroll` 使用可信指针/滚轮。派发前检查新鲜身份、整数落点及遮挡，
准备阶段的滚动和聚焦同样受取消与截止时间约束。

`key` 首版接受单个可打印字符、Enter/Tab/Escape、Backspace/Delete、Home/End、方向键、Space，
以及 Ctrl+A。其他组合与功能键明确拒绝。`type_text` 必须显式给 `mode: keys`；
默认/显式 Unicode 模式暂未实现，不能默默改为按键输入。每个字符/按键前复核焦点、身份及授权，
异常后以独立短预算调用 releaseActions，错误详情报告释放结果；正在阻塞的协议调用仍受其截止时间约束。

`key` 和单次 `click` 可给 `hold_ms: 1..5000`。按下前检查剩余预算，按住期间继续检查
文档/授权、元素身份，以及键盘焦点或原指针落点；变化、超时和取消后停止并释放。
每个键只生成一组 keyDown/keyUp，单击生成一组 pointerDown/pointerUp，不额外生成按键自动重复。
这是有界等待与协议边界检查，不承诺阻塞的浏览器命令能立即中断。

`capture` 只截取主文档中已观察、完全在视口内的节点，不自动滚动。
后端私下遍历 light/open/closed shadow 中的保护字段，在本 BiDi 会话的同一隔离 realm 内持续记住其身份；即使 password 改成 text
或移出 closed root，仍不导出值，且拒绝覆盖它的截图。私有协议响应可能含 HTML 属性，
不会将其原样导出给调用者。截图范围与任何 frame 相交也暂时拒绝；相关元素移除后可以重新观察并恢复截图。
扫描不完整则拒绝，不以未知状态放行；身份记录不跨新 provider 会话迁移，也不防止页面自己将秘密复制到其他普通内容区域。

`navigate`、`reload`、`open_tab`、`activate_page`、`close_page` 已提供，必须锚定新鲜主文档 body。
导航目标使用同一服务器 BrowserAccess 授权，重定向后的当前 URL 也重新检查。
BiDi 新建标签页与导航是两条命令：创建后再次校验源文档，私有观察并校验新空白页 body，才导航到目标。
创建后失败会在错误详情返回 `created_page_id`，保留已经发生的部分效果；调用者可重新观察该页后关闭，
后端不会在错误处理时盲目关闭可能已变化的页面。close_page 请求 `promptUnload: true`；
实际 beforeunload 提示交互 **[待验收]**，当前尚未提供 Firefox 对话框应答动作。

`back`/`forward` 只接受浏览器 Navigation API 能公开证明的相邻 HTTP(S) 历史条目。
该 API 的 entries 是当前条目周围的[连续同源片段](https://html.spec.whatwg.org/multipage/nav-history-apis.html#the-history-entry-list)，
不会把中间的跨域条目过滤后拼接。后端检查目的 URL 授权，并在隔离 realm 同一 JS turn 中
复核源和目的条目的 id/key/URL，再按确定的 key 调用 traverseTo；不按未经验证的偏移量跳转。
没有可证明的相邻条目时，节点不提供对应动作。跨域、不透明 URL、file 历史以及 API 隐去 URL 的条目
目前明确拒绝。既有长 HTTP URL 走文档授权，不受显式 navigate 的 2048 字节输入限制；不读取应用 history state。

双击仍明确拒绝：Firefox 会在每条输入命令重置点击计数，而同批两次点击不能在第一次页面处理器之后
执行本项目的 fresh 复核。该行为由本机 Firefox 159.0.1 随附 Actions.sys.mjs 和
[Mozilla Bug 1864614](https://bugzilla.mozilla.org/show_bug.cgi?id=1864614) 的协议设计记录相互印证。

尚未实现指针修饰键、hover 停留、drag/path、secret_input、上传、对话框、
跨域/不透明历史遍历、模拟环境与存储控制。它们不出现在相应节点能力中，未支持参数返回
`CONTROL.ACTION_UNSUPPORTED`。Firefox 内部特权页面也未验收，Chromium 的内部来源授权不能直接外推。

本机 Linux Firefox 159.0.1 headless 已通过公开 CLI 的 25 组检查：中文 DOM 编辑与独立值回读、
真实全选/逐键输入、按钮/滚轮事件、局部截图、无 id scope、过期/遮挡拒绝、受保护字段 reveal 后的持续保护、
移除后截图恢复，以及取消后的释放和后续点击。新增导航/刷新用独立文档 token 确认，标签页切换用
页面 visibility 回读；授权拒绝、关闭后资源消失和创建过程中源 body 被替换后的停止也通过。
历史另验收同源前进/后退、页面建立的长 URL 条目，以及跨域边界拒绝且不跳过条目。
长按新增真实事件时长、预算不足无按下、途中失焦/遮挡/取消释放与下一次输入验收。
测试见 `tests/browser/firefox_bidi_e2e.mjs`。
有界面 Firefox、Windows/macOS Firefox 与复杂 IME **[待真机验收]**；这些证据不等同于 Chromium 34 项回归。

## 桌面截图、OCR、窗口、启动、剪贴板与全局快捷键（Windows）

| 能力 | 入口 | 授权 | 语义 |
| --- | --- | --- | --- |
| 截图 | `capture` / MCP `control_capture` | 只读 | 观察过的节点重新校验后按实时边界裁剪；窗口自身的根节点截整个窗口（去掉不可见的缩放边框）。回答与浏览器相同：`format`/`data`/`width`/`height`/`clipped` + 会话加的 `digest` |
| OCR | `read_text` / MCP `control_read_text` | 只读 | 对 capture 的同一批像素跑 Windows.Media.Ocr，回答 `text`、`lines`（含 `words`），位置是元素框内比例 `box: {fx, fy, fw, fh}`，并带同一 `digest`；可选 `language`（BCP-47） |
| 窗口动作 | `act` 的 minimize / maximize / restore / close_window / activate | `--allow-input` | 只在窗口根节点上提供，按窗口当前状态与能力标注；先 UIA WindowPattern，没有时对锚定的 HWND 用 ShowWindow / WM_CLOSE；等待（有上限）状态真的达到，否则 effect unknown |
| 粘贴 | `act` 的 `paste` | `--allow-input` | 可聚焦、可写、非密码元素；见下文 |
| 启动程序 | `launch` / MCP `desktop_launch` | `--allow-input` + `--allow-launch <程序>`（可重复） | 只能启动白名单里逐字列出的程序，参数作为 argv 传入；等待（有上限）新窗口并作为 resource 返回 |
| 读剪贴板 | `clipboard_read` / MCP `clipboard_read` | `--allow-clipboard-read`（与输入授权分开，默认关） | 只读文本，`max_characters` 1..65536（默认 16384），回报完整长度；来源标了 `ExcludeClipboardContentFromMonitorProcessing`（密码管理器）时拒绝 `DRIVER.CLIPBOARD_PROTECTED` |
| 全局快捷键 | `global_key` / MCP `control_global_key` | `--allow-input` + `--allow-global-keys`（默认关） | 唯一不锚定元素的输入；见下文 |

```powershell
aad mcp --allow-input --allow-launch notepad.exe --allow-launch "C:\Tools\app.exe"
aad mcp --allow-clipboard-read                 # 只读剪贴板，不授予输入
aad mcp --allow-input --allow-global-keys      # 允许 Win+D、Alt+Tab 一类
```

未授权时本机 MCP 不在 `tools/list` 列出 `desktop_launch`、`clipboard_read`、`control_global_key`，
调用仍会到达会话并以 `POLICY.DENIED` 拒绝；远程连接总列出它们，由远端服务的授权决定。

**截图方法。** 用 `PrintWindow(PW_RENDERFULLCONTENT)` 让窗口自己渲染，而不是从屏幕 BitBlt：截图里永远不会
出现盖在目标上的其他应用窗口（不泄露别的程序的内容），目标也不必在前台。代价：最小化窗口没有画面
（拒绝 `DRIVER.WINDOW_MINIMIZED`，先 `restore`），不响应该请求的应用会渲染成黑色。密码元素拒绝
`DRIVER.PROTECTED_ELEMENT`；被滚出视野（offscreen）的元素拒绝。单边最多 8192 像素（超出 `clipped: true`）。
PNG 编码是确定的：同样的像素得到同样的 digest。

**DPI 与多显示器。** Windows 后端在第一次 UIA 调用前把进程设为 Per-Monitor v2 DPI aware：UIA 边界、窗口矩形、
SendInput 归一化用的虚拟屏幕范围和截图像素全部是物理像素。此前进程是 DPI unaware，缩放屏上落点被量化，
混合 DPI 多显示器时副屏窗口的虚拟化矩形与虚拟屏幕对不上。DPI 虚拟化的窗口（unaware，或另一缩放比的
system aware）只按逻辑尺寸渲染，截图会按物理窗口尺寸缩放，与 UIA 报告的物理边界对齐（清晰度与屏幕上一致）。
所有指针手势共用一个归一化函数，单元测试覆盖了每个像素（含负原点的虚拟屏幕）往返一致；原先 `pointer_click`
按整宽归一化，会落到左上一像素。

**指针与键盘的落点证明。** 抬窗口到前台可能被前台锁拒绝，此前仍照样发送输入，点击会落到盖在那里的东西上
（实测 fixture 的一次点击落到了 fixture 外面）。现在：指针手势发送前用 UIA `ElementFromPoint` 命中检查落点，
必须是该元素、其内部或其容器（不是桌面、兄弟或别的窗口），否则 `DRIVER.OCCLUDED`、effect not_applied，
且不说出遮挡者是谁（可能属于别的应用）；键盘（type_text、key、paste 的 Ctrl+V）发送前证明窗口在前台、
焦点在元素内，否则 `DRIVER.FOCUS_LOST`，一个键也不发。前台切换失败时退而 `AttachThreadInput` 到前台线程，不合成任何输入。

**OCR。** Windows 自带的 Windows.Media.Ocr：离线、无需安装引擎，每个已安装的 OCR 语言包一个识别器
（本机：en-US、ja、zh-Hans-CN）。没有对应语言包时 `DRIVER.OCR_UNAVAILABLE` 并在 `details.available_languages`
列出已装的。没有用 `aad-ocr`：它驱动 Tesseract，默认未安装，且在 Windows 上没有资源沙箱时拒绝运行。
识别出的行是只读输出，不是 act 的目标：要对某个词操作，就对它所在的元素 `click` 并给出该词中心的
`fraction`（fx + fw/2, fy + fh/2），这仍锚定在被观察、被重新校验的元素上；内容可能已移动时先重新读。
OCR 会混淆 I 与 l 等字符。Linux 与 macOS 用 Tesseract，见下文“其他平台”。

**粘贴。** 先复制剪贴板里所有内存型格式（GDI 句柄格式如位图、图元文件除外，位图仍以 CF_DIB 保存；超过 32 MiB
时拒绝 `DRIVER.CLIPBOARD_TOO_LARGE`，不丢用户的内容），再以延迟渲染提供文字：只有目标进程来取时才给出，
剪贴板历史、剪贴板管理器、同步工具来取一律拒绝，并带 `ExcludeClipboardContentFromMonitorProcessing`、
`CanIncludeInClipboardHistory=0`、`CanUploadToCloudClipboard=0` 让守规矩的监视者（包括 Windows 剪贴板历史）不来取。
经典 Win32 Edit/RichEdit 直接向该控件窗口发 `WM_PASTE`（消息，不受键盘钩子影响、不需前台）；其他控件在证明焦点后发
Ctrl+V。目标真正来取时才算粘贴完成，之后恢复原剪贴板；1 秒内若有同步工具把提供时的标记重新发布回来，再恢复一次。
目标始终没来取：`DRIVER.PASTE_NOT_CONSUMED`、effect unknown（快捷键已送到元素）。任何出口都会恢复剪贴板。
注意：恢复是把原内容重新写入，会成为一次新的剪贴板变更（原主程序的延迟格式此时被渲染成数据）。
实测（本机运行着 AutoHotkey、SnipDo、一个远程剪贴板同步和厂商 PC 管家）：Ctrl+V 只有约三分之一到达聚焦的 fixture Edit，
`WM_PASTE` 每次都到；同步工具曾在恢复后把提供时的标记回写，现由回写检查修复。

### 保密输入

服务操作者通过 `--allow-input --allow-secret-input --secret-env login=APP_PASSWORD` 授予
即时密码框写入，并将服务端环境变量绑定为公开名称 `login`。`aad mcp` 和 `aad control serve`
使用同一组参数；远程客户端不能代替远端服务授予权限。不要把密码字面量写在命令行中。
CLI 在启动线程与子进程之前一次性取出绑定，并移除对应环境变量，后续启动的应用、浏览器和
helper 不继承它们。这不是进程初始环境内存的擦除，也不替代系统凭据库。

先观察目标，在其 `states.protected` 为 true 且 `capabilities` 包含 `secret_input` 时，调用
`act` / `control_act`，保留完整 target，action 为：

```json
{"type":"secret_input","secret_ref":"login"}
```

它替换整个字段，绑定值为空则清空。绑定名为 1–64 个 ASCII 字母、数字、下划线或连字符，
以字母开头；值最多 1024 个 Unicode 字符，不允许 NUL 或换行，避免意外提交。
动作仍需观察引用、fresh 状态检查及输入租约。默认未授权；普通 fill/type_text/paste/key
不能写入受保护字段。输入本身不会按 Enter 提交表单。

值只在服务端绑定及平台派发时使用，不进入客户端请求、正常结果、后端错误回传或剪贴板。
成功结果仅确认已提交和已脱敏，不证明远端登录成功；调用者应观察非敏感状态来验收。
`secret_input` 暂仅用于即时会话：Step、工作流、录制保存入口及原生 workflow provider
均拒绝该动作，避免持久化明文或绕过绑定授权。系统凭据库和可恢复的保密工作流仍待实现。

Linux 使用 AT-SPI EditableText，Windows 优先用 UIA ValuePattern、必要时使用经过焦点验证的
键盘替换，macOS 使用可设置的安全 AXValue，Chromium 使用受保护表单字段的原生 value setter。
不支持的控件会拒绝，不借助剪贴板或关闭系统保护。Windows/macOS 的实际效果 **[待真机验收]**；
macOS 还需目标系统编译 Swift helper 并验证 AX/TCC 权限。

网页收到值后仍可由自身脚本复制或发送它。已输入元素的后续观察和截图保护覆盖已知 DOM 身份，
不等于能追踪页面任意派生文本；只应对用户信任的目标页面使用绑定。

**启动程序。** 不经 shell、`start` 或文件/URL 关联：可执行文件以 argv 直接启动，标准流与服务（可能承载协议本身）分离，
Windows 上控制台程序得到自己的控制台。只接受 `.exe`（`.bat/.cmd/.lnk` 等会经 cmd 或关联）；shell、脚本宿主和关联启动器
（cmd、powershell、pwsh、explorer、rundll32、mshta、wscript、sh、bash、xdg-open、open……）即使在白名单里也拒绝，因为它们的参数
会把这些都带回来。白名单项可以是绝对路径或 PATH 上的裸名；客户端必须逐字给出某一项。白名单里放解释器（python 等）
等于授予任意代码执行，由操作者负责。等待新进程的窗口（或同一映像的新窗口——启动器把工作交给另一个进程时），
返回的 resource 可直接 observe。

**全局快捷键。** 唯一不锚定元素的输入，独立动作、独立授权、默认关闭。复用 `key` 的原子 SendInput 和修饰键泄漏清理，
取输入租约，并使已有观察失效。成功时 effect 为 `dispatched`、commit_state `unknown`、`verified: false`：按键落在当时
的焦点上，没有锚定也无法验证，调用者必须重新观察，不能盲目重复。拒绝 Ctrl+Alt+Del（不能合成）与 Win+L（会锁住所有人），
输入桌面不是用户桌面（UAC 提示、锁屏）时 `DRIVER.SECURE_DESKTOP`、什么也不发。

**不支持。** UAC / 安全桌面自动化一律拒绝。窗口的 resize / move 需要坐标，不提供。

## 其他平台：Linux（KDE/X11）与 macOS

同一组会话操作、同样的授权参数与同样的回答形状（capture 的 `format`/`data`/`width`/`height`/`clipped` + `digest`，
read_text 的 `text`/`lines`/`words` 与元素框内比例 `box`）。能力只在本机真正可用时出现在 `capabilities` 与节点的
`capabilities` 中；做不到时以明确的错误码拒绝，不会假装成功。

| 能力 | Linux `desktop.linux_atspi` | macOS `desktop.macos_ax` [待验证] |
| --- | --- | --- |
| 截图 | 重新校验后 X11 root XGetImage 按实时 bounds 裁剪；protected 子树、被遮挡、非 showing 拒绝 | 只截目标窗口自身内容（CGWindowListCreateImage + 窗口 ID）；需屏幕录制权限，否则 `DRIVER.SCREEN_RECORDING_DENIED` |
| OCR | Tesseract（`aad-ocr`，`prlimit` 限制资源），装了才出现在 `reads`；否则 `DRIVER.OCR_UNAVAILABLE` | 同上，另需 `OCR_ALLOW_UNSANDBOXED_ENGINE=1`（macOS 上没有资源沙箱） |
| 窗口动作 | 顶层窗口节点；EWMH 请求（iconify、`_NET_WM_STATE` 最大化、`_NET_ACTIVE_WINDOW`、`_NET_CLOSE_WINDOW`）后读回窗口管理器的状态；没有 EWMH 窗口管理器时不提供 | AXWindow 节点；`AXMinimized`、`AXFullScreen`（maximize = 全屏）、关闭按钮、激活 + `AXRaise`，读回状态 |
| 粘贴 | X11 CLIPBOARD 由 helper 持有、只给目标进程的 X client，Ctrl+V，原内容交给一个脱离进程恢复 | 通用剪贴板延迟提供 + Concealed/Transient 标记，Cmd+V 只投给目标进程，之后恢复；无法限定来取的是谁 |
| 读剪贴板 | `x-kde-passwordManagerHint` 拒绝 | `org.nspasteboard.ConcealedType` 拒绝 |
| 全局快捷键 | XTest；拒绝 Ctrl+Alt+Backspace/Delete/L、Super+L | CGEvent；拒绝 Ctrl+Cmd+Q、Shift+Cmd+Q、Option+Cmd+Esc 与 Secure Event Input 期间 |
| 启动程序 | argv 直接启动（`setsid`），按 AT-SPI 应用 PID 或可执行文件名等待 | 同上，`.app` 写成 `X.app/Contents/MacOS/X`；不支持按 bundle id 启动 |

实测（WSL2 Ubuntu 22.04、Xvfb、openbox、Qt 5.15 fixture、tesseract 4.1，`tests/linux/run-linux-parity.sh`）：
窗口截图尺寸等于其 bounds；Tesseract 读出状态标签 “Qt fixture status idle”；粘贴后字段值为所贴文字且 xclip
读回原剪贴板；全局 Ctrl+A、Backspace 清空焦点字段；最大化后应用报告的宽度变大，最小化/还原经窗口管理器往返；
启动白名单程序后其窗口作为 resource 返回、`/bin/sh` 被拒；close_window 后该进程退出。
[待验证] KWin（真实 KDE Plasma X11）下的窗口动作与截图遮挡判断、Klipper 共存时的粘贴恢复、GTK 应用的粘贴；
macOS 一列全部需要 macOS 真机（辅助功能 + 屏幕录制权限）验证。细节见
[Linux 驱动](architecture/linux-kde-x11-driver.md) 与 [macOS 驱动](architecture/macos-ax-driver.md)。

## 观察视图与定位器

`observe` 总是保留完整捕获（`max_nodes` 上限 2000），`view` 只决定返回什么，不影响哪些节点可以 find/act：

| `view.mode` | 返回 | 适用 |
| --- | --- | --- |
| `nodes`（协议默认） | 全部节点 | 需要原始树的程序 |
| `outline`（MCP 默认） | 可操作或有名字、不在屏幕外的节点，按 `limit`（默认 80，≤500）与 `max_characters`（默认 20000）截断；回报 `matched`、`shown`、`stopped_by` | AI 阅读 |
| `overview` | 区域及其大小，不列内容 | 大窗口先看结构；Windows 窗口与浏览器页面计算区域，其他平台返回 `CONTROL.VIEW_UNSUPPORTED` |
| `text` | 只读：页面或一个区域的可见文字（`innerText`，按 `max_characters` 截断并报 `characters`、`text_truncated`），表格按行/单元格列出 | 浏览器页面读正文；其他 provider 返回 `CONTROL.VIEW_UNSUPPORTED` |

`view.region` 用于 outline 和 text，取 overview 报出的区域名，与定位器 `within` 用的名字一致；名字不存在时返回 `CONTROL.NOT_FOUND` 并在 `details.regions` 列出现有区域。
浏览器的区域是 landmark：header/nav/main/aside/footer/form/dialog、有名字的 section 和 ARIA landmark role，以可访问名命名，没有名字用 role，重名加序号；不在任何 landmark 里的节点归入 `(loose in the page)`。浏览器节点带 `parent` 与 `region`，landmark 节点的 `landmark` 就是区域名。

浏览器的 role/name 取自浏览器的可访问性树（每个元素一次 `Accessibility.getPartialAXTree`，与 `DOM.describeNode` 一起流水线发送，只采用 backend id 对得上的节点）；被忽略、内部 role（StaticText、LabelText 等）或空名字时保留启发式结果，body 始终是 `document`。开销记在 `metadata.accessibility`：实测 67 个节点 8 ms，569 个节点约 60–80 ms（整次观察 130–150 ms；顺序请求时为 321 ms）；超过 2000 个节点保留启发式。
Windows 的 outline 和 overview 直接复用本次观察的 driver 快照，不会另拍一张。

`find` 的定位器：`role`、`name`、`node_id` 在所有 provider 上精确匹配；`value`、`automation_id`、
`class_name`、`framework_id`、`states`、`match`（exact/contains）、`within`、`near`、`nth` 由 Windows
driver 求值；浏览器页面求值 `value`、`states`、`match`（contains 不区分大小写）、`within`（另一个必须唯一的定位器，或 overview 的区域名作 `name`）和 `nth`（1 起，或 first/last，按屏幕从上到下、从左到右计数，在 `within` 内计数），对 `automation_id`、`class_name`、`framework_id`、`near` 返回 `CONTROL.LOCATOR_UNSUPPORTED` 并点名字段；其他 provider 对扩展字段一律返回 `CONTROL.LOCATOR_UNSUPPORTED`，不会悄悄忽略。匹配多个时返回
`CONTROL.AMBIGUOUS`（浏览器和 macOS AX；Windows 为 `DRIVER.AMBIGUOUS_MATCH`，Linux AT-SPI 为 `DRIVER.AMBIGUOUS`）并在 `details.candidates` 列出候选；
`expect: optional` 时找不到返回 `found: false` 而不是错误，多个匹配仍然报错。

`wait` 由会话用 observe 与 find 反复实现，不依赖 provider，所以桌面与浏览器都有：给 `resource_id`、`locator` 和
`condition`（`appears` 恰好一个匹配；`disappears` 没有匹配；`state` 恰好一个匹配且其 `states` 等于给出的
`states` 对象），每隔 `interval_ms`（50..5000，默认 250）观察一次，直到满足或 `timeout_ms` 用完
（`CONTROL.WAIT_TIMEOUT`，`details` 带 `polls` 与最后一次是否存在）。多个匹配永远不算找到：一直歧义到超时返回
最后一次的歧义码（浏览器和 macOS AX 为 `CONTROL.AMBIGUOUS`，Windows 为 `DRIVER.AMBIGUOUS_MATCH`，Linux AT-SPI 为 `DRIVER.AMBIGUOUS`），`details` 带那次观察的 `observation_id`，可以在其中用候选的 `node_id` 定位。录制工作流里的 check 步骤也这样等：多个匹配既不算出现也不算消失，一直如此到期限时报歧义码。满足时返回最后一次的 `observation_id`，`appears`/`state` 还带该节点的 `target`，这次观察仍是当前观察，
可以直接 act。等待期间占用会话串行锁（单请求上限 30 秒），`cancel` 可以中止，结果为 `not_committed`。

给人用的 `aad apps/describe/overview/snapshot/find/do` 也走这个会话：每条命令一个进程、一个会话，
`aad do` 在同一个进程里观察、定位、操作，接受与 `aad find` 相同的定位参数，不跨进程传引用。
歧义候选优先用精确的 role/name 区分。字段完全相同时，使用下方持久协议入口，在该会话内重新观察，
再用同一次观察的 `observation_id` 和候选 `node_id` 定位。不要把上一条 CLI 命令的 `node_id`
传给下一条命令：重新观察后的编号可能指向另一个元素。

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
桌面窗口见上文（Windows、Linux、macOS）；不提供截图的 provider 返回 `CONTROL.ACTION_UNSUPPORTED`。

MCP 工具为 `control_capabilities`, `control_targets`, `control_observe`, `control_find`,
`control_wait`, `control_act`, `control_capture`, `control_read_text`, `control_cancel`, `control_close`，
授权时另有 `desktop_launch`、`clipboard_read`、`control_global_key`；本机未限定授权时另有 `probe_environment`、四个工作流工具和浏览器工具（含只读的 `browser_downloads`、`browser_storage`、`browser_network`）。
每次调用可带 `request_id` 与 `timeout_ms`。`control_cancel` 的被取消请求字段名为 `target_request_id`；
MCP 服务一次只处理一个调用，后面的排队；MCP cancellation notification 能取消正在执行的统一操作，排队中的调用被取消后不再执行，直接返回 `CONTROL.CANCELLED`（`not_applied`）。错误响应带 `hint`，说明可行的恢复方式。
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

MCP（`aad mcp`）一次只处理一个调用：回复上一个请求之后才开始下一个，回复按请求顺序返回。
只有 `notifications/cancelled` 会提前读取，作用于正在处理的那个控制调用；还没开始的调用不能取消。

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
（`present` 与 `within_ms`），以及可选的 `input`（fill/type_text/paste 的文字运行时传入，敏感值不落盘）。
窗口动作也可以保存为步骤（定位窗口根节点）。
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
select/check、navigate/back/forward/reload 的实际结果及导航/frame/节点过期；另有逐键输入与快捷键（记录 keydown/keypress 的 isTrusted）、指针排序与 HTML5 拖放、开放/closed shadow root 与 slot、进程内 frame 与经 `--host-resolver-rules` 映射的跨站 OOPIF（指针、键盘、输入、被页面元素遮挡时拒绝）、landmark 区域/text 视图/可访问名/扩展定位器、设备与配色模拟、cookie/localStorage 列表与清除、网络请求列表；这些在 Windows 11 本机 Chrome headless=new 上连续 5 次全部通过，Linux CI 尚未跑过新增用例 `[待验证: Linux CI 沙箱 Chromium]`；同一个测试在 Windows 上用本机 Chrome 跑过
（`AAD_TEST_CHROME` 指向 chrome.exe，临时空白 profile；本机的 Edge 用同样参数启动后立即退出，原因未查）。
隔离 Qt/AT-SPI fixture 通过真实 CLI 子进程（`aad control serve`，以及 `aad apps`、`aad do`、`aad find`）检查 fill/invoke 后的界面状态。
Windows 上 `tests/windows/verify_*_effect.py` 用自建的隔离探针窗口，经 `aad do` 检查右键、中键、双击、两个方向的滚轮、悬停、拖拽、
按键输入和保存的工作流，由 Raw Input 日志独立确认。所有 GUI fixture 串行执行。
CI 和 release 工作流使用官方 one-sdk Action 最新正式版本，不依赖 pending one-sdk PR。

Windows 真机（Windows 11、单显示器 3840×2400、250% 缩放）上，`cargo test -p aad-uia --test windows_native -- --ignored --test-threads=1`
用测试自建、自毁的窗口检查可观察效果：纯色色块截出恰好 120×80 个已知颜色像素，根节点截图包含未缩放的色块，
DPI unaware 窗口的色块截成其 300×200 物理边界；被置顶窗口盖住的元素点击被拒绝且遮挡窗口没有收到点击；
GDI 画出的 "Invoice 4217"（UIA 只看到窗口文字）被 OCR 读成一行两个词且位置正确；maximize/restore/minimize/activate/close_window
各自达到 IsZoomed/IsIconic/GetForegroundWindow/IsWindow 读回的状态；paste 后 Edit 的值由 ValuePattern 读回、剪贴板文字与之前一致；
activate 后 Shift+F9 以 F9 加 Shift 到达 fixture 窗口，Shift 随后松开。
`cargo test -p aad-control --test windows_control -- --ignored --test-threads=1` 让测试程序启动自身的一个副本：
其窗口作为 resource 返回，经会话 observe、capture 后 close_window 关闭；cmd.exe、不在白名单、只读会话的启动都被拒绝。

[待验证] 混合 DPI 的多显示器（以及虚拟屏幕负原点的真实副屏）：只有单显示器可测，归一化往返由单元测试覆盖。
[待验证] paste 的 Ctrl+V 路径（非经典控件，如 WPF、Chromium、UWP 文本框）未在真实目标上验证；本机的键盘钩子工具使 Ctrl+V 不可靠。
[待验证] `desktop_launch` 在 Linux/macOS：会话层代码可移植（`launch.rs` 已为 linux-gnu 与 apple-darwin 目标做类型检查），
按 `metadata.process_id` 匹配新的 AT-SPI 应用 / AX 应用，但未在这两个平台运行；Windows 11 记事本这类把工作交给另一进程的启动器
只按映像名匹配新窗口，未用真实应用验证。
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

### CLI 浏览器诊断

运行 `aad control diagnostics --browser-port PORT` 启动只读持久 NDJSON 会话。
在同一进程依次发送 `capabilities`、`targets`，取得响应的 session_id 和浏览器 resource_id，
随后发送（每条 JSON 独占一行）：

```json
{"request_id":"network-1","session_id":"响应中的会话ID","timeout_ms":3000,"operation":{"method":"browser_network","resource_id":"targets返回的资源ID","limit":50}}
```

`browser_network` 也可通过既有 `control serve/connect` 读取。limit 必须在 1–500，
返回脱敏请求、摘要、覆盖区间与分层丢失计数；此协议入口不读取响应正文。
采集从该会话附加页面开始，无法追回启动 CLI 之前的事件。保持进程运行并重复读取以延续采集。
结束时发送 `close`；附加的用户浏览器不会被关闭。
