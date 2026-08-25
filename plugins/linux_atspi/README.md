# Linux AT-SPI 进程驱动

`desktop.linux_atspi` 通过当前 Linux 图形会话的 AT-SPI 可访问性树提供
`inspect_session`、`list_applications`、`snapshot`、`find`、`focus`、`invoke`、`set_text`、显式
`type_text`、`toggle`、`expand` 和 `collapse`。

## 运行

```text
plugins/linux_atspi/run.sh
```

进程通过标准输入和标准输出交换 UTF-8 NDJSON。`inspect_session`、`list_applications`、`snapshot`
和 `find` 需要 `desktop.observe` 权限；七种写动作还需要
`desktop.input` 权限。

## 依赖

真实后端需要 Linux、Python 3、PyGObject，以及当前用户可访问的 AT-SPI 会话总线。
驱动优先使用 `Atspi 2.0` typelib；在 Debian/Ubuntu 系统上，它通常由
`gir1.2-atspi-2.0` 提供。若该 typelib 缺失，驱动会通过随 PyGObject 提供的
`Gio 2.0` 连接当前 session bus，调用 `org.a11y.Bus.GetAddress` 后进入当前用户的
accessibility bus。Gio fallback 支持应用枚举和只读快照，不支持任何写动作。

驱动不会扫描 `/proc`、猜测其他用户的总线地址或连接其他会话。缺少平台、依赖、
当前 session bus 或 accessibility bus 时，清单协商仍可用，动作会返回带会话与
后端诊断信息的 `DRIVER.UNAVAILABLE`。

## 安全边界

默认动作只调用 `Component.grab_focus`、`Action.do_action` 和
`EditableText.set_text_contents` 等 AT-SPI 语义接口。唯一例外是调用方明确选择的
`desktop.linux_atspi.type_text@1`：它重新抓取并精确解析 target+locator，验证原生身份、
语义指纹、进程归属和非 protected 后先聚焦，再用固定路径 C++ XTest helper 输入普通
UTF-8。`set_text`/`invoke` 永远不会自动进入该路径。驱动不做坐标点击、截图或 OCR。
只读 Gio fallback 会为全部七种写动作返回结构化的
`DRIVER.ACTION_UNSUPPORTED`。定位器只支持精确匹配；多义、过期或截断快照均失败关闭。
当前 v0 默认后端还要求进程环境明确报告 KDE、X11 和非空 `DISPLAY`；缺失这些证据，
或处于 Wayland/GNOME，会返回 `DRIVER.UNAVAILABLE`，不会扩大本切片的支持声明。
`inspect_session` 只返回 backend、session type 与 desktop 三个粗粒度字段，不读取应用树；
它是当前真实 Linux provider 唯一声明可进入 `--durable-actions read-only` 的操作。其输入、
输出、错误均声明为 public，且 checkpoint 只能显式投影这三个字段。应用列表、snapshot、
find 与全部写动作没有 durable 资格，避免窗口标题、控件文本或短期 target 落入 journal。
Gio 的 `GetChildren` 在线路响应解包后立即检查 5000 项硬上限；该上限用于阻止继续
复制和遍历异常 fan-out，但受 D-Bus API 形状限制，无法在 wire 传输之前截断响应。

`type_text` 接受 1–1024 个字符且最多 4096 UTF-8 字节，除换行外拒绝控制字符，不支持
密码或 secret。先运行 `plugins/linux_atspi/build_x11_xtest_helper.sh` 构建 helper；
Debian/Ubuntu 需要 `g++ pkg-config libx11-dev libxtst-dev`。helper 不使用 shell 拼接、
`xdotool`、剪贴板或 `uinput`，文本只经 stdin 传递。首个输入事件后任何错误或超时
都返回 `DRIVER.UNKNOWN_EFFECT` 且不得重试；成功后仍须 fresh snapshot 验证文本。
helper 的 `submitted=true` 仅表示 X server 接受了事件请求，不表示目标应用已消费文本。
登录管理器和锁屏界面不受支持；生产动作不主动解锁会话。
显式选择示例见 `examples/workflows/linux-explicit-type-text-fallback.yaml`；示例只按调用方
预先获得的 `semantic_set_text_available` 结果分支，不捕获语义动作失败后自动降级。

快照额外观察 `checked`、`expandable`、`expanded`、`selectable` 和 `selected` 状态。
本切片仅对 GTK3 明确映射语义动作：`toggle` 只接受 canonical 原生名精确等于
`click` 且 `checked` 可观察的目标；`expand`/`collapse` 只接受 canonical 原生名精确
等于 `activate`，同时要求 `expandable=true` 且 `expanded` 可观察。匹配只读取
`Action.get_action_name`，不会使用 localized name、description、大小写归一化或别名。
已处于目标展开态时 expand/collapse 返回未派发的 no-op；toggle 始终是非幂等派发。
其他 toolkit 的同名或相似动作仍需独立资格验证，不能套用 GTK3 映射。
Qt 5 Widgets 的按钮是一个单独的保守映射：只有 `push_button` 且 canonical 原生名
精确包含唯一 `Press` 时才公开 `invoke`，不会把同时存在的 `SetFocus` 当成默认动作。

本机原生测试使用 `tests/linux/atspi_fixture_app.py` 提供 GTK3 的 entry、button、
check button、expander 和 status label，在完整 PyGObject backend 可用时验证 focus 的
原生接受结果，以及 set_text、invoke、toggle、expand/collapse 的动作后重新观察。若只为
测试使用解压出的兼容 typelib，可通过
`AI_AUTO_DESKTOP_TEST_ATSPI_TYPELIB_PATH=/path/to/girepository-1.0` 显式传入；生产运行应
安装发行版提供的 `gir1.2-atspi-2.0`。
`tests/linux/qt_atspi_fixture.cpp` 则提供 Qt 5 Widgets entry/button/label。测试会按需
编译，并在同一真实 X11 display 上使用隔离的 session/AT-SPI bus，验证
snapshot/find/focus/set_text/invoke 及动作后重新观察。私有 Xvfb + 隔离 accessibility
bus 的 GTK3/Qt5 fixture 还会真实验证 `type_text` 的 XTEST UTF-8 输入和 fresh snapshot
后置条件；不使用 OCR、`xdotool`、剪贴板或坐标点击。
