# Linux KDE/X11 AT-SPI Rust 驱动

> 当前实现：`crates/aad-atspi`。首个已验证 profile 是 Linux + KDE/X11；不能外推为任意
> Linux 桌面、Wayland、XWayland 或所有 toolkit。

## 能力与实现

`desktop.linux_atspi` 由 CLI/MCP 直接注册，Rust 使用 `atspi`/`zbus` 连接当前用户 session
中的 accessibility bus。公开动作包括 `inspect_session`、`list_applications`、`snapshot`、
`find`、`focus`、`invoke`、`set_text`、`toggle`、`expand`、`collapse`，以及显式
`type_text`、`pointer_click`、`capture_target`。

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
  子树与遮挡，PNG 通过 run-scoped `ArtifactStore` 返回。

helper 不调用 shell、`xdotool`、剪贴板或 `uinput`。首次输入/指针事件后的失败按
`DRIVER.UNKNOWN_EFFECT` 处理；成功仍须 fresh snapshot 验证。截图不会自动触发 OCR，OCR
只能是 descriptor 明确声明的后续 action。

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

fake backend 单元测试继续覆盖状态映射、toggle/expand/collapse、stale/truncated、deadline、
错误归一和 Artifact 边界。既有 KDE/KCalc 探索结果属于历史资格证据；当前自动门禁以仓库内
非 Python Qt fixture 为准。登录管理器、锁屏、其他用户会话、提权界面和 Wayland 输入均不
受支持。
