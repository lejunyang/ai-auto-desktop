# Linux KDE/X11 AT-SPI 进程驱动

> 状态：首个纵向切片，2026-08-24。当前实现以 KDE/X11 为首个资格验证环境，
> 但能力清单只声明操作系统平台 `linux`；它不代表任意 Linux 桌面、Wayland、
> XWayland 或所有 Qt/QML 应用已经通过资格验证。

## 契约与支持边界

该能力提供方名为 `desktop.linux_atspi`，提供以下 v1 动作：
`list_applications`、`snapshot`、`find`、`focus`、`invoke` 和 `set_text`。
工作流中的 `uses` 由能力名、动作键和契约主版本号组成，例如
`desktop.linux_atspi.snapshot@1`。能力清单的 `runtime.platforms` 为 `linux`，
进程入口为 `./run.sh`。

`list_applications`、`snapshot` 和 `find` 只要求 `desktop.observe`；
`focus`、`invoke` 和 `set_text` 同时要求 `desktop.observe` 与
`desktop.input`。这里的 `desktop.input` 表示允许通过 AT-SPI 原生语义接口改变
应用状态，不表示允许注入键盘或指针事件。

当前切片只使用 AT-SPI 暴露的可访问性树和接口：

- `Component.grab_focus` 用于 `focus`；
- `Action.do_action` 用于 `invoke`；
- `EditableText.set_text_contents` 用于 `set_text`。

驱动不会调用 XTEST、`xdotool`、`uinput`、键鼠事件注入、截图或 OCR，也不会在
语义动作不可用时自动退化到坐标点击。AT-SPI 未暴露节点或动作时，结果会明确失败。
登录管理器、锁屏、其他用户会话和提权界面不在当前支持范围内。

## 会话和后端报告

驱动启动时只选择一个后端。Linux 上会尝试加载可选的 PyGObject `Atspi 2.0`
typelib；平台不匹配、依赖缺失、无可访问桌面或初始化失败时，能力清单协商仍然
成功，而真正的动作返回 `DRIVER.UNAVAILABLE`。不可用状态不会静默改用坐标输入。

观察结果会报告实际 backend 和会话证据，包括可获得的
`XDG_SESSION_TYPE`、`XDG_CURRENT_DESKTOP`、`DISPLAY` 与
`DBUS_SESSION_BUS_ADDRESS` 状态。环境变量只是诊断证据，不构成 KDE/X11 已经通过
资格验证的证明。尤其不能把 `DISPLAY` 存在等同于纯 X11 会话，也不能把 XWayland
窗口宣称为完整的 Wayland 支持。

真实后端当前以 `pygobject_atspi` 标识；跨平台契约测试使用注入的 fake backend。
后端抽象隔离了原生对象，因此模拟测试不会 import PyGObject，也不会要求图形会话。

## 应用枚举与快照

`list_applications` 从 AT-SPI desktop 根节点枚举应用，而不是从 X11 坐标或窗口列表
推断应用。每项包含可获得的应用名、进程 ID、toolkit 信息和当前进程内使用的
AT-SPI 身份线索。一个应用可以有多个顶层 frame、dialog 或 window 节点。

`snapshot` 接受非空的精确应用选择器。选择器只按已声明字段逐项相等比较；零个
匹配返回 `DRIVER.NOT_FOUND`，多个匹配返回 `DRIVER.AMBIGUOUS`，绝不选择第一个
应用。树抓取同时受 `max_depth` 与 `max_nodes` 限制。结果外层结构如下：

```json
{
  "snapshot_id": "<worker-generation>:<revision>",
  "revision": 1,
  "session": {"session_type": "x11", "desktop": "KDE"},
  "backend": "pygobject_atspi",
  "application": {},
  "nodes": [],
  "truncated": false
}
```

节点采用带 `parent_id` 的扁平列表，并包含 `node_id`、`role`、`name`、
`description`、`value`、`attributes`、`states`、`bounds`、`actions` 和
`provenance`。`bounds` 只是 AT-SPI `Component` 暴露的观察信息，不能用于本能力的
输入注入。受保护文本不读取、不回显，`set_text` 对此类节点失败关闭。原生对象不会
跨越 NDJSON 边界。

AT-SPI 对 Qt Widgets、QML、GTK、Electron 和自绘控件所暴露的语义完整度不同。
树中缺少名称、状态、EditableText 或 Action 接口时，驱动保留未知值或不声明对应
动作，不根据像素猜测。达到深度或节点上限、无法证明剩余子树为空时，
`truncated` 必须为 `true`。

## 精确定位与过期目标

定位器只支持区分大小写的精确匹配。它可以组合节点的 role、name、description、
value、AT-SPI 身份线索、toolkit、attributes、states 与 actions；所有已给出的条件
必须同时成立。零匹配返回 `DRIVER.NOT_FOUND`，多匹配返回
`DRIVER.AMBIGUOUS` 和有界候选摘要。截断快照无法证明全树唯一，因此 `find` 返回
`DRIVER.SNAPSHOT_TRUNCATED`。

`find` 返回限定在当前 worker 修订版中的 target：

```json
{
  "snapshot_id": "...",
  "revision": 1,
  "node_id": "n12"
}
```

再次获取快照会使上一修订版失效；worker 重启会改变 snapshot generation。每个写
动作必须同时携带 target 和原始 locator。派发前，驱动使用原快照的深度与节点数
预算重新抓树，再次精确解析 locator，并比较原生身份与语义指纹。目标消失、变成
多义、被替换或身份无法验证时返回 `DRIVER.STALE_SNAPSHOT`。旧快照或重新抓取结果
被截断时返回 `DRIVER.SNAPSHOT_TRUNCATED`。这些失败都发生在原生动作派发之前。

驱动一旦进入原生写接口，就使当前公开快照失效。原生接口报错或截止时间在派发后
耗尽时，驱动返回 `DRIVER.UNKNOWN_EFFECT`，不得自动重放。成功响应只证明 AT-SPI
调用返回成功，不证明业务后置条件成立；调用方必须获取新快照验证结果。

## NDJSON、截止时间与资源限制

worker 的 stdin/stdout 使用 UTF-8 NDJSON，一行只能包含一个 JSON 对象。stdout
只承载协议帧，诊断写入 stderr。请求上限为 1 MiB，响应必须低于宿主的 8 MiB
单帧上限；超限会返回结构化错误而不是输出部分 JSON。非法 UTF-8、非法 JSON 和
超大请求不会破坏随后合法帧的解析。

`deadline_ms` 是 Unix epoch 毫秒绝对时间，并在入口转换为单调时钟截止时间。驱动在
应用枚举、树遍历、定位和每次原生动作派发前检查截止时间。PyGObject 无法可靠抢占
一项已经阻塞的同步 D-Bus 调用，因此宿主的进程级 timeout 和 worker 回收仍是最终硬
边界。只读调用在派发前超时可安全重试；写调用进入原生接口后必须按
`UNKNOWN_EFFECT` 处理。

## 启动与资格验证

使用以下命令启动进程：

```text
plugins/linux_atspi/run.sh
```

发行版需要提供 Python 3、PyGObject、AT-SPI 2.0 typelib，以及当前用户图形会话可访问
的 AT-SPI bus。缺少其中任一条件时，应保守返回 `DRIVER.UNAVAILABLE`。当前实现不以
root 运行，也不连接其他用户的 D-Bus session。

跨平台测试通过 fake backend 验证 manifest、会话报告、快照归一化、精确与多义
定位、revision/stale、截断保护、三种语义写动作、deadline 和 NDJSON 帧限制。Linux
真机 smoke 仅在依赖与桌面确实可用时枚举应用；无 GUI、无 typelib 或无 AT-SPI bus
时保守跳过或确认 `DRIVER.UNAVAILABLE`，不会把测试环境缺失误报为驱动成功。

“KDE/X11 已资格验证”还需要在固定发行版、Plasma、Xorg 与 Qt 版本上，以 Qt Widgets、
QML、Dolphin、System Settings、Konsole、对话框、多窗口、虚拟列表和多显示器/DPI
样例分别记录语义完整度、错误目标率、动作覆盖率、延迟及挂起情况。fake backend 测试
和一次成功枚举都不构成这一发布声明。
