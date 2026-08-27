# Linux AT-SPI 进程驱动

`desktop.linux_atspi` 通过当前 Linux 图形会话的 AT-SPI 可访问性树提供
`inspect_session`、`list_applications`、`snapshot`、`find`、显式 `capture_target`、`focus`、`invoke`、显式
`pointer_click`、`set_text`、显式 `type_text`、`toggle`、`expand` 和 `collapse`。

## 运行

```text
plugins/linux_atspi/run.sh
```

进程通过标准输入和标准输出交换 UTF-8 NDJSON。`inspect_session`、`list_applications`、`snapshot`
和 `find` 需要 `desktop.observe` 权限；八种写动作还需要
`desktop.input` 权限。
`capture_target` 另需独立的 `desktop.capture` 权限；普通可访问性观察权限不包含像素读取。

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
`desktop.linux_atspi.pointer_click@1` 与 `desktop.linux_atspi.type_text@1`：二者都会重新抓取并
精确解析 target+locator，验证原生身份、语义指纹、进程归属后先聚焦，再调用固定路径 C++
XTest helper。`pointer_click` v0 只支持 `button=left`、`position=center`，并要求 fresh
snapshot 上的目标具有正面积 bounds；AT-SPI element-at-point 还必须命中目标本身或其
后代，避免同进程 sibling/overlay 覆盖中心点时误点。调用方不能传入裸 `x/y` 坐标。
`set_text`/`invoke` 永远不会自动进入这些路径。驱动不做 OCR，也不依赖 `xdotool`、剪贴板
或 `uinput`。
`capture_target` 也不是语义定位失败后的后备。它只接受已有 snapshot 中的
`target + locator + format=png`，fresh 重解析并校验身份、PID、screen bounds、protected 子树
和 X11 遮挡后，才将当前可见区域作为 Host 托管 `ArtifactRef` 返回。它禁止全屏、任意 region、
padding 和裸坐标，不包含鼠标指针，也不执行 OCR；OCR 只能是工作流中显式的后续 action。
只读 Gio fallback 会为全部八种写动作返回结构化的
`DRIVER.ACTION_UNSUPPORTED`。定位器只支持精确匹配；多义、过期或截断快照均失败关闭。
当前 v0 默认后端还要求进程环境明确报告 KDE、X11 和非空 `DISPLAY`；缺失这些证据，
或处于 Wayland/GNOME，会返回 `DRIVER.UNAVAILABLE`，不会扩大本切片的支持声明。
`inspect_session` 只返回 backend、session type 与 desktop 三个粗粒度字段，不读取应用树；
即使当前平台、会话或 AT-SPI 依赖不可用，该诊断动作仍会返回
`backend=linux_atspi_unavailable` 和环境会话字段。它不会将 list/snapshot/写操作放行；
这些动作在 backend 不可用时仍以 `DRIVER.UNAVAILABLE` 失败关闭。
它是当前真实 Linux provider 唯一声明可进入 `--durable-actions read-only` 的操作。其输入、
输出、错误均声明为 public，且 checkpoint 只能显式投影这三个字段。应用列表、snapshot、
find 与全部写动作没有 durable 资格，避免窗口标题、控件文本或短期 target 落入 journal。
Gio 的 `GetChildren` 在线路响应解包后立即检查 5000 项硬上限；该上限用于阻止继续
复制和遍历异常 fan-out，但受 D-Bus API 形状限制，无法在 wire 传输之前截断响应。

`pointer_click` 与 `type_text` 都要求先运行
`plugins/linux_atspi/build_x11_xtest_helper.sh` 构建 helper；
Debian/Ubuntu 需要 `g++ pkg-config libx11-dev libxtst-dev`。helper 不使用 shell 拼接、
`xdotool`、剪贴板或 `uinput`；`type_text` 的文本只经 stdin 传递。`pointer_click` helper
会在派发前再次验证 X focus owner PID 与点击点下窗口 PID 都属于目标进程，然后通过
XTEST 发送 move + Button1 down/up。`type_text` 接受 1–1024 个字符且最多 4096 UTF-8
字节，除换行外拒绝控制字符，不支持密码或 secret。首个输入/指针事件后任何错误或超时
都返回 `DRIVER.UNKNOWN_EFFECT` 且不得重试；成功后仍须 fresh snapshot 验证文本或点击
后置条件。helper 的 `submitted=true` 仅表示 X server 接受了事件请求，不表示目标应用已消费文本。
`pointer_click` 在指针事件前需要先聚焦目标；如果尚未提交点击但焦点已改变，失败也按
不可重试的 contextual effect 返回，不能伪装成完全没有副作用。
登录管理器和锁屏界面不受支持；生产动作不主动解锁会话。
`capture_target` 另需运行 `plugins/linux_atspi/build_x11_capture_helper.sh`。截图 helper 只依赖
Xlib，PNG 原始字节走 stdout，metadata 走一条有界 stderr JSON；不创建图片文件，也不调用
shell、ImageMagick 或外部编码器。helper 输出与 Host artifact slot 都有 64 MiB 硬上限。
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
后置条件。当前仓库还以同一套隔离 runner 真实验证 `pointer_click` 的 XTEST 左键中心点点击
及 fresh snapshot 后置条件；不使用 OCR、`xdotool`、剪贴板或调用方提供的裸坐标。
另有真实应用 runner 在禁用 TCP、使用一次性 Xauthority 的私有 Xvfb/KWin，以及私有
session/AT-SPI bus 与临时 HOME/XDG 中启动发行版 KCalc 22.12.3，每次用 fresh snapshot
精确定位 `1`、`+`、`2`、`=`，分别通过 `Action.do_action` 的 exact `Press` 和显式
`pointer_click` 完成计算。pointer 路径从语义 bounds 推导中心点，经 AT-SPI subtree
hit-test 与 X11 PID/focus 复核后用 XTEST 派发；两条路径最后都从 fresh snapshot 读取同一
显示控件的结果 `3`，且不使用 OCR 或截图。
这只证明该受控 KCalc 场景，不外推为任意 KDE 应用写动作均已通过。
