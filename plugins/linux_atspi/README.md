# Linux AT-SPI 进程驱动

`desktop.linux_atspi` 通过当前 Linux 图形会话的 AT-SPI 可访问性树提供
`list_applications`、`snapshot`、`find`、`focus`、`invoke` 和 `set_text`。

## 运行

```text
plugins/linux_atspi/run.sh
```

进程通过标准输入和标准输出交换 UTF-8 NDJSON。`list_applications`、`snapshot`
和 `find` 需要 `desktop.observe` 权限；`focus`、`invoke` 和 `set_text` 还需要
`desktop.input` 权限。

## 依赖

真实后端需要 Linux、Python 3、PyGObject，以及当前用户可访问的 AT-SPI 会话总线。
驱动优先使用 `Atspi 2.0` typelib；在 Debian/Ubuntu 系统上，它通常由
`gir1.2-atspi-2.0` 提供。若该 typelib 缺失，驱动会通过随 PyGObject 提供的
`Gio 2.0` 连接当前 session bus，调用 `org.a11y.Bus.GetAddress` 后进入当前用户的
accessibility bus。Gio fallback 支持应用枚举和只读快照，暂不支持三种写动作。

驱动不会扫描 `/proc`、猜测其他用户的总线地址或连接其他会话。缺少平台、依赖、
当前 session bus 或 accessibility bus 时，清单协商仍可用，动作会返回带会话与
后端诊断信息的 `DRIVER.UNAVAILABLE`。

## 安全边界

本驱动只调用 `Component.grab_focus`、`Action.do_action` 和
`EditableText.set_text_contents` 等 AT-SPI 语义接口。它不会注入键盘或指针事件、
不会按坐标点击、不会截图，也不会运行 OCR。受保护文本不会读取或回显，且禁止
`set_text`。只读 Gio fallback 会为 `focus`、`invoke` 和 `set_text` 返回结构化的
`DRIVER.ACTION_UNSUPPORTED`。定位器只支持精确匹配；多义、过期或截断快照均失败关闭。
当前 v0 默认后端还要求进程环境明确报告 KDE、X11 和非空 `DISPLAY`；缺失这些证据，
或处于 Wayland/GNOME，会返回 `DRIVER.UNAVAILABLE`，不会扩大本切片的支持声明。
Gio 的 `GetChildren` 在线路响应解包后立即检查 5000 项硬上限；该上限用于阻止继续
复制和遍历异常 fan-out，但受 D-Bus API 形状限制，无法在 wire 传输之前截断响应。
