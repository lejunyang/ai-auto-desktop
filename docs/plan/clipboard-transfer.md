# assist 的剪贴板互通：富文本、图片和文件两个方向

状态：设计（2026-11-18），未实现。范围是 `aad assist`（`crates/aad-cli/src/assist.rs`、`assist/viewer.html`）和各平台的剪贴板读写；MCP 的 `clipboard_read`、`paste` 动作不在这次的范围里。

## 现在是什么样

- **这边 → 远程**：viewer 拦下 Ctrl+V 的按键（`viewer.html` 里 `KeyV` 不发），在浏览器的 `paste` 事件里只取 `clipboardData.getData('text')`，发 `{"type":"text"}`。远程那边把它当输入打进去：浏览器画面走 CDP `Input.insertText`，桌面走模拟键入，最多 4096 字节。远程的剪贴板没有被动过。图片、HTML、文件都取不到文字，所以什么都不发，看起来就是“粘贴没反应”。
- **远程 → 这边**：viewer 里按 Ctrl+C / Ctrl+X 后 300 ms 发 `{"type":"clipboard"}`，远程读剪贴板的**文字**（浏览器画面读页面选区）回给 viewer，viewer 用 `navigator.clipboard.writeText` 写进本机剪贴板。`--no-clipboard` 关掉这个方向。

## 要做到的

1. 这边复制的东西在远程粘贴时**原样**到达：纯文字、富文本（HTML，拿得到 RTF 时也带上）、图片、文件（一个或多个）。
2. 远程复制的东西带回这边：文字和富文本进本机剪贴板；图片能进剪贴板时进，不能时给下载；文件给下载。
3. 走 omem 的转发（`/remote/<id>/…`）也能用，包括 omem 用 http + IP 打开的情况（不是安全上下文）。
4. 大小有上限，超了明确告诉人，不卡住画面。

## 浏览器能做什么（决定了设计）

| 能力 | 安全上下文（https、localhost / 127.0.0.1） | 不安全（http + IP 或域名） |
| --- | --- | --- |
| `paste` 事件里读 `clipboardData`：字符串类型（`text/plain`、`text/html`，有时 `text/rtf`、`text/uri-list`）和 `files` | 可以 | **也可以**：这是用户自己按的粘贴，不受安全上下文限制 |
| 粘贴的是截图 / 位图 | `files` 里一个 `image/png`（Chrome 统一转成 PNG） | 同左 |
| 粘贴的是资源管理器 / Finder 里复制的文件 | Chromium、Firefox 在 `files` 里给出每个文件（文件夹拿不到或是 0 字节的项）——**要实测** | 同左 |
| `navigator.clipboard.read/write`（异步剪贴板 API） | 可以；写支持 `text/plain`、`text/html`、`image/png`；iframe 要 `allow="clipboard-read; clipboard-write"`（omem 已加）；Chrome 要求页面有焦点，Safari / Firefox 要求在用户手势里 | **`navigator.clipboard` 是 undefined** |
| 写文字 / HTML 的退路 | — | `document.execCommand('copy')` 加 `copy` 事件里 `clipboardData.setData('text/plain' / 'text/html', …)`：要在用户激活后约 5 秒内（Chrome 的 transient activation） |
| 写图片 | 异步 API 的 `ClipboardItem({'image/png': blob})` | **做不到** |
| 把文件放进操作系统剪贴板 | **网页做不到**（任何上下文都不行） | 同左 |
| 把文件交给人 | 下载（`<a download>` 或服务端 `Content-Disposition: attachment`）；Chromium 可以把一项拖到桌面（`dataTransfer.setData('DownloadURL', 'mime:name:url')`）；`showSaveFilePicker` / `showDirectoryPicker` 只在安全上下文、只有 Chromium | 下载、拖出（`DownloadURL` 不要求安全上下文，要实测） |

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

## omem 那边（aad 发版以后）

- 远程页的“画面设置”：“在画面里复制的文字带回这边的剪贴板”改成“剪贴板互通（文字、富文本、图片和文件）”，另加“传文件上限（MB）”，对应 `--clipboard-max-mb`（`src/remote/settings.ts` 的 `viewerArgs`）。
- omem 的转发只转 GET 和 WebSocket，不转请求体：上传走 WebSocket 二进制帧，所以不用改转发。
