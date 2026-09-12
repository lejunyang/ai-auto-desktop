# Linux AT-SPI 原生能力

`desktop.linux_atspi` 已由 [`aad-atspi`](../../crates/aad-atspi) 内置到 Rust CLI 和 MCP
server，不再通过 Python/NDJSON worker 注册。Rust provider 使用 `atspi` 与 `zbus` 连接
当前用户的 accessibility bus。

公开动作包括 `inspect_session`、`list_applications`、`snapshot`、`find`、`focus`、
`invoke`、`set_text`、`toggle`、`expand`、`collapse`，以及显式的 `type_text`、
`pointer_click`、`capture_target`。观察、输入、截图分别受 `desktop.observe`、
`desktop.input`、`desktop.capture` 权限控制。所有写动作都基于未截断快照中的 target，
派发前重新抓树并核对原生 identity 与语义 fingerprint；多义、过期或无法验证时失败关闭。

## 原生 helper

AT-SPI 语义动作在 Rust 进程内直接调用。X11 键鼠输入和截图保留最小 C++ helper，因为它们
是独立的进程/效果边界：

```sh
plugins/linux_atspi/build_x11_xtest_helper.sh
plugins/linux_atspi/build_x11_capture_helper.sh
```

Debian/Ubuntu 构建依赖为 `g++ pkg-config libx11-dev libxtst-dev`。CLI 默认从同目录的
`.build/` 或发布包旁查找 helper，也可用 `AAD_LINUX_XTEST_HELPER`、
`AAD_LINUX_CAPTURE_HELPER` 指定可信路径。helper 不调用 shell、`xdotool`、剪贴板或
`uinput`。

`type_text` 和 `pointer_click` 仅支持 KDE/X11 profile，并在派发前检查焦点、PID 与
AT-SPI element-at-point；首个输入事件后的失败一律报告 `DRIVER.UNKNOWN_EFFECT`。
`capture_target` 只截取 fresh 语义 target 的可见区域，经 Host 的 run-scoped
`ArtifactStore` 返回 PNG，不接受任意坐标或全屏截图，也不会隐式调用 OCR。

## 验证

CI 使用纯 Rust driver 和 [`qt_atspi_fixture.cpp`](../../tests/linux/qt_atspi_fixture.cpp)，
在私有 Xvfb/session bus/AT-SPI bus 中验证应用枚举、snapshot/find、语义写值、XTest
Unicode 输入、中心点点击和目标截图：

```sh
tests/linux/run-rust-atspi-fixture.sh
```

该 fixture 证明受控 Qt5/X11 路径，不把结果外推到任意桌面、Wayland、锁屏或提权界面。
完整边界见 [`linux-kde-x11-driver.md`](../../docs/architecture/linux-kde-x11-driver.md)。
