# macOS Accessibility（AX）Rust 驱动

> 当前实现：`crates/aad-macos-ax` + 代码签名 Swift helper。Rust 负责公开 provider 语义；
> Swift 只保留 TCC/AX/CGEvent 原生边界。

## 进程与信任边界

```text
Rust runtime / CLI / MCP
  │ in-process Provider calls
  ▼
aad-macos-ax (manifest, validation, snapshots, locators, effects)
  │ private bounded NDJSON, opaque native tokens
  ▼
MacOSAXHelper.app (AppKit / ApplicationServices / CGEvent / TCC identity)
```

公开动作是 `list_apps`、`snapshot`、`find`、`focus`、`invoke`、`set_value`、显式
`type_text`、`pointer_click`、`pointer_gesture`、`drag`、`key_chord`、`paste`，窗口动作
`minimize`/`maximize`/`restore`/`close_window`/`activate`，只读的 `capture`，以及不锚定元素、
需单独授权的 `clipboard_read`、`global_key`。观察需要 `desktop.observe`，写动作还需要
`desktop.input`。helper 协议版本 3（加入后几项；Rust adapter 与 helper 必须同版本）。仓库不包含 Python adapter，也没有 PyObjC/AppleScript fallback。

Rust adapter 只接受固定 `.app/Contents/MacOS/MacOSAXHelper` 结构，启动前执行
`codesign --verify --strict` 并核对 bundle ID、可执行名与 package type。完整性检查不等于
发布者认证；自定义 helper 明确标为 `custom_untrusted/source_authenticated=false`。
`AI_AUTO_DESKTOP_MACOS_AX_HELPER` 可覆盖 helper 路径，部署者需另行建立来源信任。

## AX snapshot 与写前复核

`list_apps` 只枚举当前 Aqua session 的应用并报告 Accessibility trust。`snapshot` 要求精确
app selector，只从所选 PID 创建 AX root；BFS 遍历受 depth/node 上限约束。protected 值保持
redacted，native `AXUIElement` 只存在 helper 的短期 token store 中。

locator 是区分大小写的 exact matching；零匹配、多匹配和截断快照都失败关闭。写动作必须
携带 current target 与原 locator。Rust adapter 以原预算 fresh capture、重新唯一定位并比较
语义 fingerprint；helper 再用 `CFEqual` 核对新旧 AX 对象。身份变化或无法证明一致时不派发。

`focus`/`set_value` 预检属性可写性，`invoke` 预检 `AXPress`。`type_text` 仅对安全文本角色
开放，先确认 Secure Event Input 未开启、目标已聚焦且应用仍在前台，再分块提交 Unicode
CGEvent。`pointer_click` 从 fresh bounds 计算中心点，要求 AX hit-test 回到同一元素且应用仍
frontmost，再提交 mouse moved/down/up。二者都不接受裸坐标，也不会作为其他动作的隐式
fallback。

helper 在首个键盘或鼠标事件前发送 progress marker。marker 前的失败保持 not-applied 或
contextual；marker 后的 timeout、EOF、协议错误、部分响应和无法确认统一为不可重试的
`DRIVER.UNKNOWN_EFFECT`。成功只证明系统 API 接受提交，调用方仍须 fresh AX snapshot 验证
业务结果。

## 截图、窗口、粘贴、剪贴板与全局快捷键

以下全部 [待验证]：本仓库开发机不能编译 Swift 或运行 TCC，需要 macOS 13+ 真机、
给 `MacOSAXHelper.app` 授予辅助功能与（截图时）屏幕录制权限；CI 的 macOS job 只证明 helper 能编译、签名并握手。

| 能力 | 实现 | 拒绝 / 限制 |
| --- | --- | --- |
| `capture` | 与写动作相同的 fresh 复核；子树中有 protected 元素即拒绝。helper 找到元素所在 AXWindow，在 `CGWindowListCopyWindowInfo` 中按 PID、layer 0 与 frame 唯一匹配其 CGWindowID，用 `CGWindowListCreateImage(…, IncludingWindow, id, BoundsIgnoreFraming|BestResolution)` 只截该窗口自身内容（盖在上面的别的窗口不进入图像），裁剪到元素 bounds（单边 ≤8192 点）；像素为 Retina 物理像素 | `CGPreflightScreenCaptureAccess()` 为假时 `DRIVER.SCREEN_RECORDING_DENIED`（不弹授权框）；最小化 `DRIVER.WINDOW_MINIMIZED`；找不到唯一窗口 `DRIVER.CAPTURE_FAILED`。该函数在新 SDK 中对新部署目标标为废弃/淘汰，helper 运行时用 `dlsym` 查找；系统不再提供时同码拒绝（未实现 ScreenCaptureKit） |
| `read_text` | 控制会话对 capture 运行 Tesseract（`aad-ocr`） | macOS 上 aad-ocr 没有资源沙箱，只有操作者设置 `OCR_ALLOW_UNSANDBOXED_ENGINE=1` 且装了 tesseract 时才在 `reads` 中出现，否则 `DRIVER.OCR_UNAVAILABLE` |
| 窗口动作 | 只在 AXWindow 节点上，按当前状态提供：`minimize` 设 `AXMinimized`；`maximize` 设 `AXFullScreen`（macOS 没有与 Windows 相同的最大化，绿色按钮的“缩放”状态无法可靠读回，因此不用）；`restore` 取消最小化或全屏；`close_window` 按 `AXCloseButton`；`activate` 激活应用并 `AXRaise`。每项最多等 3 秒读回状态（窗口从应用 `AXWindows` 消失、成为前台应用的 `AXMainWindow`） | 状态没达到 `DRIVER.UNKNOWN_EFFECT`（例如关闭前弹出保存表单）；属性不可写时不提供 |
| `paste` | 可聚焦、`AXValue` 可写、非 secure 的元素。保存通用剪贴板全部条目（≤32 MiB，否则 `DRIVER.CLIPBOARD_TOO_LARGE`），以带 `org.nspasteboard.ConcealedType`/`TransientType` 标记、延迟提供 `.string` 的条目写入，证明目标应用在前台、元素已聚焦、Secure Event Input 关闭后向目标进程 `postToPid` Cmd+V；只有在有人来取文字时才算粘贴完成，之后若剪贴板未被别人改写则恢复原条目 | 没有人来取：`DRIVER.PASTE_NOT_CONSUMED`、effect unknown。macOS 不告诉提供方是谁来取，所以不能像 Windows 那样只给目标进程 |
| `clipboard_read` | 通用剪贴板 `.string`，有界 | 带 `org.nspasteboard.ConcealedType`（密码管理器）时 `DRIVER.CLIPBOARD_PROTECTED` |
| `global_key` | CGEvent 组合键投递到 HID event tap，不锚定元素 | Secure Event Input 开启时拒绝；拒绝 Ctrl+Cmd+Q（锁屏）、Shift+Cmd+Q（注销）、Option+Cmd+Esc（强制退出）`DRIVER.KEY_RESERVED` |
| `launch` | 会话直接以 argv 启动白名单里的可执行文件（`setsid`；`.app` 需写成 `X.app/Contents/MacOS/X`），按 `list_apps` 的 PID / `process_name`（`executableURL` 的文件名）等待应用出现 | `open`、shell 等拒绝；没有实现按 bundle id 经 NSWorkspace 启动 |

## 构建和验证

```sh
plugins/macos_ax/build.sh
cargo run --locked -q -p aad-macos-ax --example list_apps
```

macOS CI 构建并签名 helper，再通过 Rust adapter 做握手和应用枚举 smoke。Rust fake-helper
测试在所有平台验证 manifest、snapshot/locator、stale/protected、deadline、progress marker
和 unknown-effect 归一。

`tests/macos/` 另有无 Python 的 Swift 真机套件，覆盖自有 AppKit fixture 的 AXFocused、
AXValue、AXPress、Unicode CGEvent、secure text 拒绝与中心点 pointer 回读。当前 Linux 开发机
不能执行 TCC/Apple framework，因此不能用本机验证替代真实 Mac 资格证据。

## 明确边界

- 首次使用需给 `MacOSAXHelper.app` Accessibility 权限；provider 不主动弹窗。
- 不创建 system-wide AX root；`list_apps` 只报告应用可执行文件的文件名。截图只截目标窗口自身，
  需要已授予的屏幕录制权限，helper 从不主动请求。
- 不支持锁屏、安全输入、跨用户 session 或任意第三方应用的泛化保证。
- ad-hoc 重签可能改变 TCC 身份；稳定部署应使用固定 Developer ID 和固定安装路径。
