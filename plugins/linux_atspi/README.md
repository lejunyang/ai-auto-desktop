# Linux AT-SPI 进程驱动

`desktop.linux_atspi` 通过当前 Linux 图形会话的 AT-SPI 可访问性树提供
`list_applications`、`snapshot`、`find`、`focus`、`invoke`、`set_text`、`toggle`、
`expand` 和 `collapse`。

## 运行

```text
plugins/linux_atspi/run.sh
```

进程通过标准输入和标准输出交换 UTF-8 NDJSON。`list_applications`、`snapshot`
和 `find` 需要 `desktop.observe` 权限；六种写动作还需要
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

本驱动只调用 `Component.grab_focus`、`Action.do_action` 和
`EditableText.set_text_contents` 等 AT-SPI 语义接口。它不会注入键盘或指针事件、
不会按坐标点击、不会截图，也不会运行 OCR。受保护文本不会读取或回显，且禁止
`set_text`。只读 Gio fallback 会为全部六种写动作返回结构化的
`DRIVER.ACTION_UNSUPPORTED`。定位器只支持精确匹配；多义、过期或截断快照均失败关闭。
当前 v0 默认后端还要求进程环境明确报告 KDE、X11 和非空 `DISPLAY`；缺失这些证据，
或处于 Wayland/GNOME，会返回 `DRIVER.UNAVAILABLE`，不会扩大本切片的支持声明。
Gio 的 `GetChildren` 在线路响应解包后立即检查 5000 项硬上限；该上限用于阻止继续
复制和遍历异常 fan-out，但受 D-Bus API 形状限制，无法在 wire 传输之前截断响应。

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
snapshot/find/focus/set_text/invoke 及动作后重新观察；它同样不使用 OCR、XTEST 或
虚拟键盘鼠标。
