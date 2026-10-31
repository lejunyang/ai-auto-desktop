# Linux KDE/X11 AT-SPI Rust 驱动

> 当前实现：`crates/aad-atspi`。首个已验证 profile 是 Linux + KDE/X11；不能外推为任意
> Linux 桌面、Wayland、XWayland 或所有 toolkit。

## 能力与实现

`desktop.linux_atspi` 由 CLI/MCP 直接注册，Rust 使用 `atspi`/`zbus` 连接当前用户 session
中的 accessibility bus。公开动作包括 `inspect_session`、`list_applications`、`snapshot`、
`find`、`focus`、`invoke`、`set_text`、`toggle`、`expand`、`collapse`，以及显式
`type_text`、`pointer_click`、`pointer_gesture`、`drag`、`key_chord`、`paste`、`capture_target`、
`capture`、窗口动作 `minimize`/`maximize`/`restore`/`close_window`/`activate`，和不锚定元素、
需单独授权的 `clipboard_read`、`global_key`。

生产代码不扫描 `/proc` 寻找其他用户的 bus，不猜测其他 session；环境、session bus、AT-SPI
registry 或平台 profile 不满足要求时返回 `DRIVER.UNAVAILABLE`。`inspect_session` 仍可返回
有界诊断，但不会把其他动作伪装成可用。

## snapshot、locator 与写前复核

应用按 PID/name/toolkit 等已声明字段精确选择；零匹配和多匹配都失败关闭。树抓取受
`max_depth`、`max_nodes` 和单次 children fan-out 硬上限约束。节点保留 AT-SPI bus/object path
identity、语义字段、状态、bounds、动作和 provenance；protected 值不读取。

`find` 只接受未截断快照。写动作必须同时携带 target 与原 locator，driver 以原预算 fresh
capture，重新唯一定位，并核对原生 identity、进程归属和语义 fingerprint。元素消失、变成
多义、被替换或无法证明一致时不会派发。`toggle`、`expand`、`collapse` 只在状态与 canonical
native action 同时满足受测映射时公开；Qt Widgets 按钮仅在唯一 `Press` 可用时公开 invoke。

## X11 helper 与 Artifact

AT-SPI 语义调用在 Rust 进程内完成。显式键鼠输入与截图使用固定路径的最小 C++ helper：

- `type_text`：聚焦并验证 PID 后，以 XTEST 提交有界 UTF-8 文本；
- `pointer_click`：由 fresh bounds 计算中心点，经 AT-SPI subtree hit-test 与 X11 PID/focus
  复核后提交左键；
- `capture_target`：只截取 fresh 语义 target 的可见矩形，拒绝全屏、任意坐标、protected
  子树与遮挡，PNG 通过 run-scoped `ArtifactStore` 返回；`capture` 是同一检查与截图，
  直接返回 base64 PNG，供控制会话的 `control_capture` 使用，并把调用者的观察放回（读不消耗观察）。
  遮挡按堆叠顺序判断：窗口管理器 frame 里位于 client 之下的装饰/背景窗口不算遮挡，之上的才算。

helper 不调用 shell、`xdotool` 或 `uinput`；剪贴板只在 `paste` 与 `clipboard_read` 中经 X11
selection 协议访问（见下文）。首次输入/指针事件后的失败按 `DRIVER.UNKNOWN_EFFECT` 处理；
成功仍须 fresh snapshot 验证。截图不会自动触发 OCR。

## 截图、OCR、窗口、粘贴、剪贴板与全局快捷键

与 Windows 同一组会话操作与授权（`--allow-input`、`--allow-clipboard-read`、
`--allow-global-keys`、`--allow-launch`），只在本平台可做到的范围提供：

| 能力 | 实现 | 拒绝 / 限制 |
| --- | --- | --- |
| `capture` | 上述 X11 root XGetImage，按 fresh bounds 裁剪 | protected 子树、被别的窗口或子树外可见节点覆盖、非 showing 均拒绝；最小化窗口不可截 |
| `read_text` | 会话对 capture 的 PNG 运行 `aad-ocr`（Tesseract，`prlimit` 资源限制）；BCP-47 映射为 traineddata（en→eng、zh-Hans→chi_sim 等） | 没有 tesseract 或 prlimit 时 `reads` 不列 `read_text`，调用返回 `DRIVER.OCR_UNAVAILABLE`；缺语言数据时同码并列出 `available_languages` |
| 窗口动作 | 顶层窗口节点（应用节点的直接子节点：Qt 主窗口是 frame，裸 QWidget 窗口是 filler）。helper 找到该进程 `_NET_WM_PID` 且根坐标几何等于该节点 bounds 的唯一 client 窗口，向窗口管理器发 `WM_CHANGE_STATE`（XIconifyWindow）、`_NET_WM_STATE` 最大化/还原、`_NET_ACTIVE_WINDOW`、`_NET_CLOSE_WINDOW`，再（最多 3 秒）读回 `_NET_WM_STATE` / `WM_STATE` / `_NET_ACTIVE_WINDOW` / 窗口是否消失 | 只在有 EWMH 窗口管理器（`_NET_SUPPORTING_WM_CHECK`）时按当前状态提供；状态没达到返回 `DRIVER.UNKNOWN_EFFECT`；找不到唯一窗口 `DRIVER.STALE_SNAPSHOT` |
| `paste` | 聚焦后 helper 先保存 CLIPBOARD 现有全部 target（单项不超过一次请求、合计 ≤32 MiB，否则 `DRIVER.CLIPBOARD_TOO_LARGE` 不粘贴），以真实时间戳占有 CLIPBOARD，只向拥有目标进程焦点窗口的同一 X client（`_NET_WM_PID`，或与该窗口同一 resource-id 基址——Qt/GTK 的取数窗口没有 pid 属性）提供文字，证明焦点后发 Ctrl+V；之后由一个脱离的 restorer 进程持有原内容直到别人占有剪贴板（最长 1 小时） | 目标始终没来取：`DRIVER.PASTE_NOT_CONSUMED`、effect unknown；protected 元素不提供；原先没有 owner 时恢复为无 owner |
| `clipboard_read` | 读 CLIPBOARD 的 UTF8_STRING（否则 STRING），有界 | 带 `x-kde-passwordManagerHint`（KDE / KeePassXC 的密码标记）时 `DRIVER.CLIPBOARD_PROTECTED`；INCR 大选区 `DRIVER.CLIPBOARD_TOO_LARGE` |
| `global_key` | XTest 组合键发给当前焦点，不锚定元素 | 拒绝 Ctrl+Alt+Backspace（X zap）、Ctrl+Alt+Delete、Ctrl+Alt+L、Super+L（`DRIVER.KEY_RESERVED`）；结果 `verified: false` |
| `launch` | 会话直接以 argv 启动白名单程序（`setsid`，不经 shell），按 AT-SPI 应用的 PID（或 `/proc/<pid>/exe` 的文件名）等待其出现 | shell、`xdg-open`、`gio` 等即使在白名单里也拒绝；不可执行文件拒绝 |

restorer 恢复的是内容的拷贝：原 owner 的延迟格式此时已被取成数据，这会被剪贴板管理器视为一次新的变更。

构建 helper：

```sh
plugins/linux_atspi/build_x11_xtest_helper.sh
plugins/linux_atspi/build_x11_capture_helper.sh
```

## 验证

CI 在私有 Xvfb、session bus 和 AT-SPI bus 中编译 Qt5 C++ fixture，并使用正式 Rust provider
验证应用枚举、snapshot/find、set_text、XTest Unicode 输入、中心点点击和目标截图：

```sh
tests/linux/run-rust-atspi-fixture.sh
```

截图、OCR、窗口、粘贴、剪贴板、全局快捷键与启动另有私有 Xvfb + D-Bus + openbox（EWMH 窗口管理器）
中的效果验证，每一项都以可观察结果判定（截图尺寸等于窗口 bounds、Tesseract 读出标签文字、粘贴后
字段值变化且 xclip 读回原剪贴板、全局 Ctrl+A/Backspace 清空焦点字段、最大化后应用自己报告的
宽度变大、最小化/还原经窗口管理器往返、启动的白名单程序作为 resource 出现、close_window 后进程退出）：

```sh
tests/linux/run-linux-parity.sh
```

实测（WSL2 Ubuntu 22.04、Xvfb、openbox 3.6、Qt 5.15、tesseract 4.1）全部通过。
[待验证] KWin（真实 KDE Plasma X11 会话）下的窗口动作、截图遮挡判断与 Klipper 共存时的粘贴恢复；
GTK 应用的 paste 取数窗口归属（需 GTK3/4 fixture）。

fake backend 单元测试继续覆盖状态映射、toggle/expand/collapse、stale/truncated、deadline、
错误归一和 Artifact 边界。既有 KDE/KCalc 探索结果属于历史资格证据；当前自动门禁以仓库内
非 Python Qt fixture 为准。登录管理器、锁屏、其他用户会话、提权界面和 Wayland 输入均不
受支持。
