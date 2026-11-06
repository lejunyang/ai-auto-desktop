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

XTest 输入和目标截图要求明确的 `XDG_SESSION_TYPE=x11` 与可连接的 `DISPLAY`，不以 KDE、GNOME、
XFCE 等桌面名称决定是否可用。Wayland 会话仍拒绝，即使存在 XWayland 的 `DISPLAY`。
输入仍在派发前检查目标 PID、同用户身份以及对应的焦点或 AT-SPI element-at-point
（`drag` 起点终点都查）；窗口管理动作另需窗口管理器提供相应 EWMH 能力。首个输入事件后的失败
一律报告 `DRIVER.UNKNOWN_EFFECT`，helper 失败时会先松开已按下的键和按钮。`key_chord` 只接受
修饰键（ctrl、shift、alt、super）加一个主键（字母数字、F1–F24、方向、Enter、Tab、Esc 等命名键）。
`capture_target` 只截取 fresh 语义 target 的可见区域，经 Host 的 run-scoped
`ArtifactStore` 返回 PNG，不接受任意坐标或全屏截图，也不会隐式调用 OCR。

`drag.path` 支持同一观察、同一元素内最多 32 个中间锚点，起终点及每个锚点均用归一化比例。
要求 helper 声明 `anchored-drag-path`；缺失时拒绝该组合，空路径保留原拖拽行为。
按下前检查全部点，每段移动前重新抓取 AT-SPI 树并检查前后点，释放前再核对终点；helper
同时验证已绑定 X11 窗口的 PID、几何和命中。窄 scope 的路径额外读取所属完整窗口用于遮挡
检查（最多 5000 节点、128 层，截断则拒绝），不扩大可操作成员。可见的其他分支覆盖锚点时
保守拒绝，不假设 toolkit 的命中结果或节点顺序能证明堆叠关系。

该私有协议通过请求唯一 nonce 和严格序号配对验证/ACK。只读 AT-SPI worker 即使阻塞，
helper 和主循环仍独立检查取消与截止时间，先释放本次持有的键鼠；迟到 worker 不再发送 ACK。
窗口完整树不等于像素级遮挡证明，未暴露在无障碍树中的覆盖层仍受 toolkit 可见性和 X11
窗口命中能力限制。逐段完整抓树也有开销：本机小 Qt fixture 的两中间点路径约 0.7 秒，
surface scope 加完整窗口检查约 1.1 秒，不代表复杂应用或 32 点路径的性能。

## 验证

CI 使用纯 Rust driver 和 [`qt_atspi_fixture.cpp`](../../tests/linux/qt_atspi_fixture.cpp)，
在私有 Xvfb/session bus/AT-SPI bus 中验证应用枚举、snapshot/find、语义写值、XTest
Unicode 输入、中心点点击和目标截图，并经 `aad do` 验证双击和 Ctrl+A 后输入：

```sh
tests/linux/run-rust-atspi-fixture.sh
```

该 fixture 证明受控 Qt5/X11 路径，不把结果外推到任意桌面、Wayland、锁屏或提权界面。
完整边界见 [`linux-kde-x11-driver.md`](../../docs/architecture/linux-kde-x11-driver.md)。

`pwsh -File tests/linux/verify-x11-session.ps1` 在私有 Xvfb 中切换桌面名称，回读实际点击事件和目标
PNG，并确认 PID、键盘焦点、Wayland 会话拒绝条件仍生效。它验证协议条件，不代表真实桌面管理器
的完整验收。**[待验证] GNOME X11 / XFCE 真机**还需分别检查应用文本输入、焦点切换、遮挡截图、
窗口激活和剪贴板恢复；当前真实桌面效果证据来自 KDE Plasma/X11。

`pwsh -File tests/linux/verify-x11-drag-path.ps1` 在私有 Xvfb 独立回读实际轨迹，并检查坏 ACK、
EOF、超时、取消、窗口移动、遮挡和已有按键的拒绝/释放；
`pwsh -File tests/linux/verify-native-drag-path.ps1 -Aad target/debug/aad -InputHelper /absolute/path/to/x11_xtest_helper`
使用新建 Qt 窗口，通过公开控制入口验证逐段锚点、完整窗口和窄 scope、真实 sibling overlay
与取消释放，并输出独立事件、动作结果及耗时。
