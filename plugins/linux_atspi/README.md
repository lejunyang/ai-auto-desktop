# Linux AT-SPI 原生能力

`desktop.linux_atspi` 已由 [`aad-atspi`](../../crates/aad-atspi) 内置到 Rust CLI 和 MCP
server，不再通过 Python/NDJSON worker 注册。Rust provider 使用 `atspi` 与 `zbus` 连接
当前用户的 accessibility bus。

公开动作包括 `inspect_session`、`list_applications`、`snapshot`、`find`、`focus`、
`invoke`、`set_text`、`toggle`、`expand`、`collapse`，以及显式的 `type_text`、
`pointer_click`、`pointer_gesture`（右键、中键、双击、滚轮、悬停，位置是元素内比例）、`drag`、
`key_chord`、`capture_target`。观察、输入、截图分别受 `desktop.observe`、
`desktop.input`、`desktop.capture` 权限控制。所有写动作都基于未截断快照中的 target，
派发前重新抓树并核对原生 identity 与语义 fingerprint；多义、过期或无法验证时失败关闭。

录制用 `watch` / `collect` / `release`：AT-SPI 信号报告文本修改、勾选和焦点，XTest helper 的
`record-input` 用 XRecord 报告落在目标进程窗口上的点击、滚轮和按键（只读，不注入），合成步骤见
[统一控制与远程会话](../../docs/control-sessions.md#录制)。

## 原生 helper

AT-SPI 语义动作在 Rust 进程内直接调用。X11 键鼠输入和截图保留最小 C++ helper，因为它们
是独立的进程/效果边界：

```sh
plugins/linux_atspi/build_x11_xtest_helper.sh
plugins/linux_atspi/build_x11_capture_helper.sh
```

Debian/Ubuntu 构建依赖为 `g++ pkg-config libx11-dev libxtst-dev`。CLI 默认从同目录的
`.build/` 或发布包旁查找 helper，也可用 `AAD_LINUX_XTEST_HELPER`、
`AAD_LINUX_CAPTURE_HELPER` 指定可信路径。helper 不调用 shell、`xdotool` 或 `uinput`。

XTest helper 另有 `window-info` / `window-action`（EWMH 窗口动作并读回状态）、`paste`（持有 CLIPBOARD、
只给目标进程、Ctrl+V、交给脱离的 restorer 恢复原内容）、`clipboard-read` 与 `global-key`；控制会话的
`capture` 复用截图 helper 并直接返回 PNG，`read_text` 对它运行 Tesseract。语义与限制见
[`linux-kde-x11-driver.md`](../../docs/architecture/linux-kde-x11-driver.md)，效果验证：

```sh
tests/linux/run-linux-parity.sh   # 另需 openbox x11-utils xclip tesseract-ocr tesseract-ocr-eng
```

`type_text`、`pointer_click`、`pointer_gesture`、`drag`、`key_chord` 仅支持 KDE/X11 profile，
并在派发前检查焦点、PID 与 AT-SPI element-at-point（`drag` 起点终点都查）；首个输入事件后的失败
一律报告 `DRIVER.UNKNOWN_EFFECT`，helper 失败时会先松开已按下的键和按钮。`key_chord` 只接受
修饰键（ctrl、shift、alt、super）加一个主键（字母数字、F1–F24、方向、Enter、Tab、Esc 等命名键）。
`capture_target` 只截取 fresh 语义 target 的可见区域，经 Host 的 run-scoped
`ArtifactStore` 返回 PNG，不接受任意坐标或全屏截图，也不会隐式调用 OCR。

## 验证

CI 使用纯 Rust driver 和 [`qt_atspi_fixture.cpp`](../../tests/linux/qt_atspi_fixture.cpp)，
在私有 Xvfb/session bus/AT-SPI bus 中验证应用枚举、snapshot/find、语义写值、XTest
Unicode 输入、中心点点击和目标截图，并经 `aad do` 验证双击和 Ctrl+A 后输入：

```sh
tests/linux/run-rust-atspi-fixture.sh
```

该 fixture 证明受控 Qt5/X11 路径，不把结果外推到任意桌面、Wayland、锁屏或提权界面。
完整边界见 [`linux-kde-x11-driver.md`](../../docs/architecture/linux-kde-x11-driver.md)。
