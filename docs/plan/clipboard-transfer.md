# assist 的剪贴板互通：富文本、图片和文件两个方向

状态：已实现（2026-11-18）：Windows 桌面、浏览器页面（CDP）、Linux X11 桌面已在真机 / Xvfb 上验证，macOS 桌面写好未经真机验证；实测结果和与设计不同的地方见文末“实测”。范围是 `aad assist`（`crates/aad-cli/src/assist.rs`、`assist/viewer.html`）和各平台的剪贴板读写；MCP 的 `clipboard_read`、`paste` 动作不在这次的范围里。

## 现在是什么样

- **这边 → 远程**：viewer 拦下 Ctrl+V 的按键（`viewer.html` 里 `KeyV` 不发），在浏览器的 `paste` 事件里只取 `clipboardData.getData('text')`，发 `{"type":"text"}`。远程那边把它当输入打进去：浏览器画面走 CDP `Input.insertText`，桌面走模拟键入，最多 4096 字节。远程的剪贴板没有被动过。图片、HTML、文件都取不到文字，所以什么都不发，看起来就是“粘贴没反应”。
- **远程 → 这边**：viewer 里按 Ctrl+C / Ctrl+X 后 300 ms 发 `{"type":"clipboard"}`，远程读剪贴板的**文字**（浏览器画面读页面选区）回给 viewer，viewer 用 `navigator.clipboard.writeText` 写进本机剪贴板。`--no-clipboard` 关掉这个方向。

## 要做到的

1. 这边复制的东西在远程粘贴时**原样**到达：纯文字、富文本（HTML，拿得到 RTF 时也带上）、图片、文件（一个或多个）。
2. 远程复制的东西带回这边：文字和富文本进本机剪贴板；图片能进剪贴板时进，不能时给下载；文件给下载。
3. 走 omem 的转发（`/remote/<id>/…`）也能用，包括 omem 用 http + IP 打开的情况（不是安全上下文）。
4. 大小有上限，超了明确告诉人，不卡住画面。

## 浏览器能做什么（决定了设计）

（“实测”标注的格子是 2026-11-18 在 Windows 11 + Chrome 157（可见窗口）上量的，见文末；其余是设计时的判断。）

| 能力 | 安全上下文（https、localhost / 127.0.0.1） | 不安全（http + IP 或域名） |
| --- | --- | --- |
| `paste` 事件里读 `clipboardData`：字符串类型（`text/plain`、`text/html`，有时 `text/rtf`、`text/uri-list`）和 `files` | 可以（实测：`text/plain`、`text/html`） | **也可以**：这是用户自己按的粘贴，不受安全上下文限制（实测：和安全上下文完全一样） |
| 粘贴的是截图 / 位图 | `files` 里一个 `image/png`，名字 `image.png`（Chrome 统一转成 PNG；实测：剪贴板里只有注册格式 `PNG` 没有 `CF_DIB` 时 Chrome 什么都不给，截图工具两种都放所以不受影响） | 同左（实测） |
| 粘贴的是资源管理器 / Finder 里复制的文件 | Chrome（Windows）在 `files` 里给出每个文件，名字、类型、大小都对，`types` 是 `["Files"]`（实测）；文件夹会得到读不出的项，查看页读第一个字节试出来并提示“请打包”；Firefox、Finder 未测 | 同左（实测，Chrome） |
| `navigator.clipboard.read/write`（异步剪贴板 API） | 可以；写支持 `text/plain`、`text/html`、`image/png`；iframe 要 `allow="clipboard-read; clipboard-write"`（omem 已加）；Chrome 要求页面有焦点，Safari / Firefox 要求在用户手势里 | **`navigator.clipboard` 是 undefined** |
| 写文字 / HTML 的退路 | — | `document.execCommand('copy')` 加 `copy` 事件里 `clipboardData.setData('text/plain' / 'text/html', …)`：要在用户激活后约 5 秒内（Chrome 的 transient activation） |
| 写图片 | 异步 API 的 `ClipboardItem({'image/png': blob})` | **做不到** |
| 把文件放进操作系统剪贴板 | **网页做不到**（任何上下文都不行） | 同左 |
| 把文件交给人 | 下载（`<a download>` 或服务端 `Content-Disposition: attachment`；实测文件和 zip 都下载到了）；Chromium 可以把一项拖到桌面（`dataTransfer.setData('DownloadURL', 'mime:name:url')`；实测 `dragstart` 照常触发、`DownloadURL` 设上了，放到资源管理器里落没落地**没测成**，见文末）；`showSaveFilePicker` / `showDirectoryPicker` 只在安全上下文、只有 Chromium | 下载（实测）、拖出（实测 `dragstart` 同样触发；落地同样没测成） |

结论：

- **这边 → 远程在任何地址下都能完整做到**：`paste` 事件拿得到字符串格式和文件，不依赖安全上下文。
- **远程 → 这边**：文字和 HTML 在任何地址下都能进本机剪贴板（不安全时用 `execCommand` 退路）；图片只有安全上下文能进剪贴板，否则给预览和下载；文件永远是下载（加 Chromium 的拖出）。viewer 要知道自己是不是安全上下文（`window.isSecureContext`），在界面上说清图片为什么只能下载。

## 这边 → 远程

### viewer

- `paste` 事件：收集 `clipboardData.types` 里的全部字符串类型（跳过 `Files`）和 `clipboardData.files`。
  - 只有 `text/plain`、不超过 4096 字节、而且服务端没开剪贴板互通（`--no-clipboard`）时，走现在的 `{"type":"text"}` 键入。其余一律走下面的粘贴。
  - 开了剪贴板互通时纯文字也走粘贴：换行、制表符、输入法相关的字符都原样到达，不受键入的 4096 字节限制。
- 发 `{"type":"paste","id":"<随机>","formats":{"text/plain":"…","text/html":"…"},"files":[{"name":"a.png","type":"image/png","size":12345}]}`。字符串合计不超过 8 MB，超了只带 `text/plain` 并提示。
- 文件内容走二进制帧（见“协议”）。上传时画面角上显示进度（“正在传 3 个文件，45 MB，60%”）和“取消”。服务端回 `{"type":"paste","id":..,"state":"received|pasted|failed","message":..}`。
- 粘贴前先检查大小，超过 `limits.files`（服务端在 `ready` 里告诉 viewer）直接提示，不上传。
- 只有正在控制的 viewer 能粘贴（和输入同样的规则）。

### 远程

- 文件写到本次 assist 的临时目录：`<系统临时目录>/aad-assist-<pid>/paste/<id>/<文件名>`。文件名去掉路径分隔符和保留名，重名加序号；assist 退出时删除整个目录（启动时清理上次异常退出留下的）。
- 把这次粘贴的全部格式一起放进剪贴板，再按一次粘贴键（Windows / Linux 是 Ctrl+V，macOS 是 Cmd+V，按 `ready` 里的 `os`）：
  - 文字：平台的文本格式。
  - HTML：Windows 是 `HTML Format`（CF_HTML，带 `StartHTML/EndHTML/StartFragment/EndFragment` 偏移头），macOS 是 `public.html`，X11 是 `text/html` target。
  - RTF（拿得到时）：`Rich Text Format` / `public.rtf` / `text/rtf`。
  - 图片（`image/*` 的文件）：**同时**放位图格式和文件列表。Windows 是 `PNG` 注册格式 + `CF_DIBV5`，macOS 是 `public.png`，X11 是 `image/png`。浏览器分不清“复制的截图”和“复制的一个 png 文件”（两者在 `files` 里一模一样），两种格式都放，目标程序挑自己认的：Word、聊天软件拿位图，资源管理器拿文件。
  - 文件：Windows 是 `CF_HDROP`（`DROPFILES` 结构，宽字符路径）加 `Preferred DropEffect` = `DROPEFFECT_COPY`；macOS 是一组 `public.file-url`；X11 是 `text/uri-list` 加 `x-special/gnome-copied-files`（`copy\nfile:///…`）。
- **粘贴后恢复远程原来的剪贴板**，沿用 `aad-windows/src/clipboard.rs` 的 `Paste` 的做法（延迟渲染，只把数据交给前台的目标进程，等它取完再恢复，其他进程来要一律拒绝）：远程机器上的人复制的东西不被覆盖，剪贴板历史和同步工具也拿不到这次粘贴的内容。`Paste` 现在只处理文字，要扩展成多格式。X11 的 selection owner（`x11_xtest_helper.cpp`）和 macOS helper 同理扩展。
  - 例外：文件粘贴进资源管理器时，资源管理器可能在按键返回后才异步读 `CF_HDROP`，等待目标取数据的超时要比文字长（比如 10 秒），超时后再恢复。
- 浏览器画面（CDP）：
  - 字符串格式和图片：先试“真粘贴”——`Browser.grantPermissions(["clipboardReadWrite","clipboardSanitizedWrite"])`，在页面里 `navigator.clipboard.write([new ClipboardItem({...})])`，再 `Input.dispatchKeyEvent` 带 `commands: ["paste"]`。这是受信任的粘贴，页面里的编辑器照常处理。**要先验证** headless / 新 headless 下这条路通不通。
  - 不通时，或有非图片文件时：在页面里用收到的数据造一个 `DataTransfer`（字符串 + `File`），对焦点元素派发 `new ClipboardEvent('paste', {clipboardData, bubbles: true, cancelable: true})`。没人处理（没有 `preventDefault`）且焦点在可编辑元素上时：有 HTML 用 `document.execCommand('insertHTML')`，否则 `Input.insertText`。
  - 焦点在 `<input type=file>` 上时，用 `DOM.setFileInputFiles` 把临时目录里的文件交给它。
  - 文件内容进页面要转成 base64 经 `Runtime.evaluate` / `callFunctionOn` 传：浏览器画面的单次粘贴上限比桌面小（比如 20 MB），超了提示。

## 远程 → 这边

### 远程

- 触发：viewer 里按 Ctrl+C / Ctrl+X（现在的做法），以及 viewer 上新加的“远程剪贴板”按钮（随时看远程剪贴板里有什么，也给 viewer 一个新的用户手势去写剪贴板）。
- 读剪贴板的全部格式，回一份清单：
  `{"type":"clipboard","text":"…","html":"…","truncated":false,"image":{"id":"…","width":..,"height":..,"size":..},"files":[{"id":"…","name":"a.pdf","size":..,"dir":false}]}`
  - `text`、`html` 直接带在消息里（各自不超过 8 MB，超了截断并标 `truncated`）。
  - 图片只给 id 和尺寸，内容由 viewer 按需取。
  - 文件列表来自 `CF_HDROP` / `public.file-url` / `text/uri-list`：只给名字和大小，内容按需取；文件夹标 `dir`，下载时打成 zip。
  - 浏览器画面：先 `Input.dispatchKeyEvent` 带 `commands: ["copy"]` 让页面自己复制，再用授权后的 `navigator.clipboard.read()` 读出 `text/plain`、`text/html`、`image/png`；不通时退回现在的读选区（只有文字）。
- 内容走 HTTP：`GET /clipboard/<id>?token=…`（同一个 token，同一个手写的 HTTP 服务），`Content-Type` 按类型，文件带 `Content-Disposition: attachment; filename*=UTF-8''…`，文件夹和“全部下载”实时打成 zip 流式返回。id 是 128 位随机数，只对最近一份清单有效（新的清单作废旧的），只在 assist 进程活着时有效。
- 读的时候按上限检查：单个文件、合计都不超过 `limits.files`，超了的在清单里标 `tooLarge`，viewer 显示但不给下载。
- 读剪贴板时尊重 `ExcludeClipboardContentFromMonitorProcessing`（Windows 上密码管理器的标记，`clipboard_read` 已经这样做）：有这个标记时整份不读，回 `{"type":"clipboard","protected":true}`。

### viewer

- `text`、`html`：
  - 安全上下文：`navigator.clipboard.write([new ClipboardItem({'text/plain': …, 'text/html': …})])`。
  - 不安全：`execCommand('copy')` 退路，在 `copy` 事件里 `setData` 两种格式。清单在 Ctrl+C 之后一两秒内回来，还在用户激活的 5 秒窗口里；超时或失败时提示“点这里复制”，用这次点击作为新手势再写。
- 图片：
  - 安全上下文：和文字放进同一个 `ClipboardItem`，`'image/png'` 的值是 `fetch('clipboard/<id>?token=…').then(r => r.blob())` 的 Promise（Chrome 接受 Promise，写入不用等下载完）。
  - 不安全：画面角上弹出“远程剪贴板”托盘，显示缩略图、“下载”，以及一句“这个地址不是 https，浏览器不让网页把图片放进剪贴板”。
- 文件：托盘里逐个列出，名字、大小、“下载”，多于一个时有“全部下载（zip）”。Chromium 上每一项都能直接拖到桌面或文件夹（`dragstart` 里设 `DownloadURL`）。安全上下文下另给“保存到…”（`showSaveFilePicker` / `showDirectoryPicker`），可以一次存一整个文件夹。
- 托盘不自动弹出盖住画面：只有文字和 HTML 的复制只在角上闪一下“已复制到本机”；有图片（且写不进剪贴板）或有文件时才展开托盘，点别处收起，“远程剪贴板”按钮再打开。

## 协议

在现有协议上加，老的 viewer 和老的 assist 互相不认识新消息时照旧工作：

- `ready` 加 `"clipboard":{"paste":true,"files":true,"limits":{"text":8388608,"files":104857600,"count":100}}`。没有这个字段的 assist 只支持文字，viewer 退回现在的行为。
- viewer → 服务端：
  - `{"type":"paste","id",…}`（见上）。
  - 二进制帧：`"AADP"`（4 字节）、版本 u8 = 1、保留 3 字节、paste id 长度 u16 加 id、文件序号 u16、偏移 u64，后面是这一块数据。一块不超过 256 KB。服务端每收满 4 MB 回 `{"type":"paste","id","ack":<已收字节>}`，viewer 在途不超过 8 MB，不让大文件把画面帧挤住。
  - `{"type":"paste","id","cancel":true}`。
- 服务端 → viewer：`{"type":"paste",…}` 的进度和结果；新的 `{"type":"clipboard",…}` 清单（向下兼容：`text`、`characters`、`truncated` 字段保持原样）。

viewer 现在只发文本帧，服务端读帧的循环要能收二进制帧（目前收到会怎么处理要看一下）。

## 选项和上限

- `--no-clipboard`：两个方向都只剩文字键入，不读远程剪贴板（和现在的含义一致，只是扩大到新功能）。
- `--clipboard-max-mb <N>`：一次粘贴或一份清单里文件的合计上限，默认 100；0 表示只互通文字、富文本和图片，不传文件。
- 单次粘贴最多 100 个文件；文件夹在“这边 → 远程”方向不支持（浏览器给不出来），提示“文件夹请打包后再复制”。
- `--view-only` 下全部关掉。

## 分阶段

每一段做完、测过就提交（按本仓库 AGENTS.md）：

1. 协议和 viewer 的粘贴上传（二进制帧、进度、取消、上限）；Windows 桌面的多格式粘贴（文字、HTML、RTF、图片、文件，粘贴后恢复）。
2. Windows 桌面的反向：读多格式、清单、`GET /clipboard/<id>`、zip；viewer 写剪贴板（安全 / 不安全两条路）、托盘、下载、拖出。
3. 浏览器画面（CDP）两个方向。
4. Linux X11（扩展 selection helper）、macOS（扩展 AX helper 的剪贴板）。
5. 文档：`docs/control-sessions.md` 或单独一页写清能力表（哪个平台、哪个方向、哪些格式）；README 的 assist 部分。

## 验证

- Windows 桌面：在记事本、写字板 / Word（HTML、RTF、图片）、画图（图片）、资源管理器（文件）里各粘贴一次；粘贴后远程原来的剪贴板还在；剪贴板历史（Win+V）里没有这次粘贴的内容。
- viewer 分别从 `http://127.0.0.1:<port>`（安全上下文）和 `http://<局域网 IP>:<port>`（不安全）打开，两个方向各跑一遍，确认上面表格里每一格的实际表现，结果写回这份文档（尤其是“粘贴资源管理器里复制的文件”和“`DownloadURL` 拖出”在不安全上下文下的表现）。
- 100 MB 文件上传时画面帧照常刷新（帧率不掉到 0）；超上限的被拒绝并提示；取消后临时文件被删除。
- 经 omem 转发打开（`/remote/<id>/`）再测一遍下载：omem 的 `proxyHttp` 会把 `Content-Disposition` 等响应头原样转回来，GET 不用改 omem。
- 已有的 `tests/windows/verify_assist_input.py` 一类脚本加上粘贴和反向复制的检查。

## 实测（2026-11-18）

环境：这台 Windows 11（150% 缩放）、Chrome 157 可见窗口、Word（Microsoft 365）、资源管理器；查看页分别从
`http://127.0.0.1:<port>`（安全上下文，`navigator.clipboard` 存在）和 `http://192.168.31.185:<port>`
（经一个 TCP 转发，模拟 omem 用 IP 打开；`isSecureContext` 为 false，`navigator.clipboard` 是 undefined）打开。
Linux 在 WSL Ubuntu 22.04 里自起 Xvfb + openbox。

这边 → 远程（两个地址结果一样）：

- Word：文字（制表符、换行、emoji、中文）原样；HTML 保留粗体；只给 RTF 时保留粗体；一张图片插成图片
  （`InlineShape` 类型 3，图片而不是嵌入的文件对象）。
- 资源管理器：资源管理器里复制的两个文件（含 2 MB 的）经查看页粘贴进另一个文件夹，逐字节相同；100 MB 的文件
  经协议直接上传约 3.7 秒，期间画面照常（10 fps 下 38 帧，最长间隔 0.16 s），落地逐字节相同。
- 每次粘贴后剪贴板仍是原来的内容；Win+V 历史里没有粘贴过的内容（用 WinRT 读历史核对）。
- 浏览器页面（无头 Chrome）：授权 + `navigator.clipboard.write` + 带 `commands:["paste"]` 的 Ctrl+V **在无头下是通的**：
  页面的 `paste` 事件是受信任的，带 `text/plain`、`text/html` 和 `image/png` 文件，contenteditable 插入了粗体。
  PDF 走合成的 `paste` 事件（`isTrusted` 为 false），`files` 里是 `doc.pdf`；`<input type=file>` 拿到两个文件；
  超过 20 MB 被拒绝。
- X11：焦点窗口的 Xlib 程序收到全部 target（gnome-copied-files、uri-list、html、rtf、png、UTF8_STRING…），内容一致；
  粘贴后 xclip 原来放的内容还在。

远程 → 这边：

- 安全上下文：Chrome 把文字、HTML 和 PNG 一起写进了本机剪贴板（剪贴板所有者是 chrome.exe，格式有
  PNG、CF_DIBV5、HTML Format、CF_UNICODETEXT）。
- 不安全：`execCommand('copy')` 退路写进了文字和 HTML；图片进托盘，有下载和“不是 https”的说明。
- 文件、文件夹（zip）、“全部下载”（zip）从两个地址都下载到了，内容正确；`Content-Disposition` 带
  `filename*=UTF-8''…`，中文文件名正确。
- 浏览器页面：选区（含粗体）复制回来是文字 + 带内联样式的 HTML。X11：xclip 放的 HTML、PNG、文件都取回来了。

与设计不同、被实测推翻或补上的地方：

- **粘贴线程在按键期间必须继续答复渲染请求**（Windows）：按设计“放好格式 → 按键 → 等目标来取”，但按键要回到主循环
  去按，粘贴线程在等的时候不处理消息；Word 收到 Ctrl+V 后立刻来取，请求得不到答复就拿到空数据，结果粘贴出
  空白或纯文字。改成在另一个线程按键、粘贴线程一直处理消息之后 8/8 保留粗体。
- **Ctrl 和 V 不能紧挨着发**：同一个 `SendInput` 序列里 Ctrl 按下后马上 V，Word 收到的是打出一个 “v”；间隔 50 ms
  就是粘贴。粘贴键现在每步间隔 30 ms。（这也说明靠 `modifiers` 字段补按的快捷键在这台机器上可能有同样的问题，
  不在这次范围。）
- **浏览器把大的二进制消息分片发**：256 KB 的上传块被 Chrome 拆成多个 WebSocket 帧，服务端原来不拼接分片，
  第二块起偏移对不上。服务端现在按 RFC 6455 拼接分片，分片之间的 ping 照常答复。
- **X11 不在 `x11_xtest_helper.cpp` 里做**：assist 的 X11 桌面直接用 x11rb 说 X 协议，从不启动那个 helper，所以
  选择所有者写在 `aad-linux` 里（自己的连接、自己的线程）；判断“目标进程”用焦点窗口和服务器的 resource-id 掩码。
- Windows 剪贴板保持的时间：没有按设计给 HTML/图片更长的等待；修好上面第一条后 400 ms 足够，文件仍是 1.5 s。
- 测试上的坑（不是产品问题）：两边是同一台机器时，测试刚往剪贴板放的 HTML 会触发 Word 自己的剪贴板监听，紧接着
  粘贴有约三分之一的概率变成纯文字；真实使用中查看端的复制不会到达远程的 Word。Word 启动几秒后会弹出账户对话框抢
  焦点，测试会先关掉它。无头 Chrome 的 `paste` 事件读不到系统剪贴板，查看页的测试必须用可见窗口。

没有测到的：

- 把托盘里的一项拖到资源管理器（`DownloadURL`）：两个地址都看到 `dragstart` 触发、`DownloadURL` 设上，但自动化没能
  稳定地把放置目标摆在鼠标下，落地与否未确认。[待验证] 需要人手拖一次。
- Firefox、Safari、Edge 的查看页；Finder 里复制的文件。
- macOS 桌面的全部（JXA 读写 NSPasteboard）：只在这里交叉编译、过了 clippy。[待验证] 需要 macOS 13+ 真机，在
  TextEdit / Pages 里粘贴 HTML、RTF、图片，在 Finder 里粘贴文件，并从它们复制回来。
- 经 omem 转发（`/remote/<id>/`）下载：没有在 omem 里测；`GET` 走 omem 的 `proxyHttp`，按设计不需要改 omem。
- 画图（新版画图是商店应用，这台机器上没有 `mspaint.exe`）、写字板（Windows 11 已移除）。

## X11：别的程序抢剪贴板（2026-11-19）

0.0.9 发版后在一台经 xrdp 连进去的 KDE Plasma 5（X11）上，从查看页往 Edge 地址栏粘贴纯文字也失败（“没有粘贴：…”），
往回复制正常；同一台机器上浏览器页面（CDP）的粘贴是好的，所以问题在 X11 桌面这条路。那台机器上有 xrdp-chansrv（RDP
剪贴板同步）和多半有 Klipper 在盯 CLIPBOARD，原来 Xvfb + openbox 的测试里都没有。

复现：`tests/linux/verify_assist_paste_contention.py`，私有 Xvfb + openbox + Chrome for Testing 158（页面把文本框里
落下的字写进窗口标题，用 xdotool 读），`tests/linux/x11_clipboard_grabber.c` 模拟抢剪贴板的程序：

- `chansrv DELAY`：每次 CLIPBOARD 换主人就读新主人的 TARGETS，DELAY 毫秒后自己拿走（xrdp-chansrv 在 RDP 客户端宣布
  它的剪贴板时就是这样）；`chansrv DELAY 1` 只拿一次。
- `klipper`：换主人时读 TARGETS，没有文字就拿回自己的最后一条（Klipper 的“防止剪贴板为空”）；带
  `x-kde-passwordManagerHint` 的不碰。

0.0.9 的结果：没有抢的、`klipper` 都粘贴成功（只给 Klipper 标记，它不动）；`chansrv 0`（立刻抢）时 Chrome 粘贴进去
的是抢的那个程序的内容，aad 却报“前台程序没有接受粘贴”；`chansrv 50`（50 ms 后抢）时 Chrome 收到了 `paste` 事件但
没有字落下，aad 却报“已粘贴”；`chansrv 300` 成功。也就是说，**按键前后剪贴板被别的程序拿走**会让粘贴失败、粘贴
错东西，报告还不对。Chrome 来要数据的窗口和 `_NET_ACTIVE_WINDOW` 同属一个 client，在 openbox 下没有出现“认错目标”。

另一个会让粘贴卡住的地方：粘贴前先把原来的剪贴板逐个格式读出来（为了事后恢复），每个格式最多等 2 秒；xrdp 下原来的
主人是 chansrv，它每个格式都要到 RDP 客户端去取，格式一多粘贴就要等很久，查看页一直显示“上一次粘贴还在传”。

改成：

- 读原来的剪贴板总共最多 1.5 秒，读不完的就不恢复。
- 拿到 CLIPBOARD 后先**连续保持 150 ms 再按键**；期间被拿走就拿回来，最多 6 次。保持不住就不按键（按了会粘贴别人的
  内容），报告 `reason: "contended"`。按键之后、目标读到之前被拿走，也拿回来（再最多 6 次）；保持不住报 `lost`，目标
  一直没来要报 `not_asked`。报告里带 `takers`（谁拿走的：窗口和 WM_CLASS）、`asked`（谁来要了什么、给没给）和
  `saved_ms`，也放进给查看页的答复的 `detail` 里，真机上看得出是哪一种。
- 判断目标 client 除了 `_NET_ACTIVE_WINDOW` 也认输入焦点所在窗口的 client（窗口管理器把激活窗口和焦点放在不同 client
  的窗口上时不至于把目标当外人）；两个都拿不到时谁来要都给。
- **纯文字（没有图片、文件）剪贴板没粘上时改为直接输入**（0.0.9 之前的做法，走 `{"type":"text"}` 同一条输入路径），
  最多 64 KB；答复 `state: "pasted"` 带 `typed: true` 和原因 `code`。带 HTML 的也会输入它的纯文字。
- 失败答复带 `code`（`contended` / `lost` / `not_asked`），查看页按自己的语言说原因；英文 `message` 仍在。
- 查看页：一次粘贴 20 秒（文件还在传时 60 秒）没收到任何确认或状态，就取消它、清掉并提示，不再挡住之后的粘贴；
  连接断开时也清掉。

那台机器上还开着 CrossPaste（跨设备剪贴板同步）。读了它的源码（`LinuxPasteboardService.kt`，2025-02 起的各版）：
X11 下它只用 XFixes 看换主人、经 AWT 读，`lostOwnership` 只记一下，**丢了剪贴板不会抢回来**；2026-09-17（#5015）起认
`x-kde-passwordManagerHint`。它写 CLIPBOARD 只在两种时候：用户在它的面板里选了一条，或者**别的设备同步过来一条**。
如果查看端那台电脑也开着 CrossPaste，人在那边一复制，同样的内容就会被同步到这台 Linux 上、在不确定的时刻占一次
CLIPBOARD，正好落在 aad 粘贴的过程中时 0.0.9 就报“没有粘贴”。所以复现里加了：在某个时刻只抢一次（100 ms：
落在按键前的保持期里；250 ms：按键之后），以及 `retake DELAY`：从一开始就占着、每次丢了 DELAY 毫秒后抢回、不理会
密码管理器标记的程序（比 CrossPaste 更凶的情况）。

改后同一套复现 24 种情况全部落字：不抢、`klipper`、`chansrv 300`、只抢一次（立刻、100 ms、250 ms）、`retake 400`
走剪贴板，HTML 也是剪贴板给的；`chansrv 0`、`chansrv 50`、`retake 0 / 20 / 100`（150 ms 内一直抢）改为直接输入。原来的 `verify_assist_clipboard_x11.py` 照常通过。
查看页的超时和中英文原因在 Chrome 里用控制台造了一个卡住的粘贴核对过。

[待验证] 真正的 xrdp-chansrv、Klipper 和 KWin（这里装不了 KDE，没有 root）：需要在那台 KDE Plasma + xrdp 机器上从查看页
往 Edge 地址栏、Kate 里粘贴文字和带格式的文字，看答复里的 `takers` / `asked`；RDP 客户端是否会在 chansrv 宣布之后
再宣布一次（一直抢还是只抢一次）决定了带格式的内容能不能保留。omem 自己画的查看页要跟着认 `code`、`typed` 和超时。

## omem 那边（aad 发版以后）

- 远程页的“画面设置”：“在画面里复制的文字带回这边的剪贴板”改成“剪贴板互通（文字、富文本、图片和文件）”，另加“传文件上限（MB）”，对应 `--clipboard-max-mb`（`src/remote/settings.ts` 的 `viewerArgs`）。
- omem 的转发只转 GET 和 WebSocket，不转请求体：上传走 WebSocket 二进制帧，所以不用改转发。
