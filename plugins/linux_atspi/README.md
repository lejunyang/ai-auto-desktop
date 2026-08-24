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

真实后端需要 Linux、Python 3、PyGObject、`Atspi 2.0` typelib，以及当前用户
可访问的 AT-SPI 会话总线。在 Debian/Ubuntu 系统上，typelib 通常由
`gir1.2-atspi-2.0` 提供。缺少平台、依赖、图形会话或总线时，清单协商仍可用，
动作会返回带会话与后端诊断信息的 `DRIVER.UNAVAILABLE`。

## 安全边界

本驱动只调用 `Component.grab_focus`、`Action.do_action` 和
`EditableText.set_text_contents` 等 AT-SPI 语义接口。它不会注入键盘或指针事件、
不会按坐标点击、不会截图，也不会运行 OCR。受保护文本不会读取或回显，且禁止
`set_text`。定位器只支持精确匹配；多义、过期或截断快照均失败关闭。
