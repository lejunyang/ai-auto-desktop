# 统一控制与远程会话

`aad-control/1` 为 computer-use 与 browser-use 提供同一组操作：
`capabilities → targets → observe → find → act`，另有 `wait`、`cancel` 和 `close`。
它不依赖模型厂商的控制 API。平台适配器继续执行自己的实时元素校验；不支持的动作明确拒绝。

## 支持的动作与入口

| provider | 类型化动作 | 实际语义 / 限制 |
| --- | --- | --- |
| `desktop.windows_uia` | focus, invoke, click, fill, type_text, key, scroll, hover, drag, paste, minimize, maximize, restore, close_window, activate | UIA + 原生输入；click 带 button/count/fraction 时走原生手势；节点必须暴露对应能力。指针动作先对落点做 UIA 命中检查，键盘动作先证明窗口在前台、焦点在元素内（见下文）。另有只读的 capture / read_text，以及需单独授权的 launch、clipboard_read、global_key |
| `desktop.linux_atspi` | focus, invoke, click, fill, type_text, key, scroll, hover, drag, paste, minimize, maximize, restore, close_window, activate | AT-SPI；指针/键盘经 X11 XTest helper，只支持 KDE/X11 会话；派发前核对焦点、PID 与落点，drag 起止两点都核对；窗口动作只在顶层窗口节点上、需要 EWMH 窗口管理器。另有只读 capture / read_text（Tesseract），以及需单独授权的 launch、clipboard_read、global_key（见下文“其他平台”） |
| `desktop.macos_ax` | focus, invoke, click, fill, type_text, key, scroll, hover, drag, paste, minimize, maximize, restore, close_window, activate | AX helper；需要同一登录用户的辅助功能权限；指针动作要求目标应用在前台并经 AX 命中检查，事件只投递给目标进程；key 名称按字面（`ctrl` 是 Control，`cmd` 是 Command）；窗口动作只在 AXWindow 节点上，maximize 即全屏。另有 capture（需屏幕录制权限）、read_text、launch、clipboard_read、global_key。[待验证] 全部新增项需 macOS 真机 |
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

下载：浏览器级连接建立时（第一次 `targets` 或标签页动作）用 `Browser.setDownloadBehavior`（`allow`，`eventsEnabled`）
把下载放进本会话专用的临时目录 `aad-downloads-<uuid>`，并只缓冲 `Browser.downloadWillBegin/downloadProgress` 事件。
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
`ms`（1..5000，默认 500）、`fraction`；`drag` 要 `to`（同一次观察里另一个节点的 target），可带
`button`、`fraction`、`to_fraction`；`select` 要 `option`（选项文字或值，观察节点的 `options` 列出文字）；
`check` 要 `checked`；`navigate` 与 `open_tab` 要 `url`；`accept` 可带 `text`（≤4096）；`paste` 要 `text`（1..4096）。`fraction` 是元素框内 0..1 的比例，不是屏幕坐标。
`capabilities` 每个 provider 另列 `reads`（`capture`、`read_text`），并只在授权时列出 `launch`（允许启动的程序）、
`clipboard_read: true`、`global_keys: true`。

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
桌面窗口见上文（Windows、Linux、macOS）；不提供截图的 provider 返回 `CONTROL.ACTION_UNSUPPORTED`。

MCP 工具为 `control_capabilities`, `control_targets`, `control_observe`, `control_find`,
`control_wait`, `control_act`, `control_capture`, `control_read_text`, `control_cancel`, `control_close`，
授权时另有 `desktop_launch`、`clipboard_read`、`control_global_key`；本机未限定授权时另有 `probe_environment`、四个工作流工具和浏览器工具（含只读的 `browser_downloads`）。
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
select/check、navigate/back/forward/reload 的实际结果及导航/frame/节点过期；同一个测试在 Windows 上用本机 Chrome 跑过
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
