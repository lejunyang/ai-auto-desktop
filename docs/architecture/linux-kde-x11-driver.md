# Linux KDE/X11 AT-SPI 进程驱动

> 状态：首个纵向切片，2026-08-24。当前实现以 KDE/X11 为首个资格验证环境，
> 但能力清单只声明操作系统平台 `linux`；它不代表任意 Linux 桌面、Wayland、
> XWayland 或所有 Qt/QML 应用已经通过资格验证。

## 契约与支持边界

该能力提供方名为 `desktop.linux_atspi`，提供以下 v1 动作：
`list_applications`、`snapshot`、`find`、显式 `capture_target`、`focus`、`invoke`、显式
`pointer_click`、`set_text`、显式 `type_text`、`toggle`、`expand` 和 `collapse`。
工作流中的 `uses` 由能力名、动作键和契约主版本号组成，例如
`desktop.linux_atspi.snapshot@1`。能力清单的 `runtime.platforms` 为 `linux`，
进程入口为 `./run.sh`。

`list_applications`、`snapshot` 和 `find` 只要求 `desktop.observe`；`capture_target`
同时要求 `desktop.observe` 与独立的 `desktop.capture`；
全部八种写动作同时要求 `desktop.observe` 与 `desktop.input`。只有调用方明确选择
`pointer_click` 或 `type_text` 时，`desktop.input` 才授权本切片的受限 XTest 注入。

当前切片只使用 AT-SPI 暴露的可访问性树和接口：

- `Component.grab_focus` 用于 `focus`；
- `Action.do_action` 用于 `invoke`；
- `Component.grab_focus` 后由固定路径 C++ XTest helper 注入 move + Button1 down/up，用于显式
  `pointer_click`；
- `EditableText.set_text_contents` 用于 `set_text`。
- `Component.grab_focus` 后由固定路径 C++ XTest helper 注入普通 UTF-8，用于显式
  `type_text`。
- GTK3 上，`Action.do_action` 的 exact canonical `click` 用于 `toggle`，exact
  canonical `activate` 用于 `expand`/`collapse`。

除显式 `pointer_click` 与 `type_text` 外，驱动不会调用 XTEST；`set_text`、`invoke` 或
其他语义动作失败时也绝不会自动进入这些路径。`pointer_click` v0 只接受
`target + locator`，只支持 `button=left` 与 `position=center`，显式拒绝调用方提供的
裸 `x/y` 坐标。派发前 AT-SPI element-at-point 必须命中目标本身或目标子树，随后 helper
再核对 X focus owner 和点击点下窗口 PID；同进程 sibling/overlay 不会仅凭 PID 一致而放行。
helper 不调用 `xdotool`、shell、`uinput` 或剪贴板，驱动不做 OCR。
AT-SPI 未暴露节点、bounds 或动作时，结果会明确失败。
登录管理器、锁屏、其他用户会话和提权界面不在当前支持范围内。

## 受控目标截图

`capture_target@1` 只接受先前 `find` 返回的 target、同一 exact locator 与 `format=png`。
它复用写动作的 fresh 重解析、原生 identity 与语义 fingerprint 校验，并要求 fresh 快照未截断、
应用与节点 PID 一致、目标及已枚举后代的 protected/value-redacted 都明确为 false、screen bounds
为正面积。调用方不能传全屏、任意 region、padding 或裸坐标。

固定路径 X11 helper 在当前 euid 内再次核对 PID 与窗口层级，要求 region 完整落在 root 和唯一
目标顶层窗口内，并拒绝更高顶层窗口相交。它在短暂 server grab 内复核场景、从 root
`XGetImage` 读取当前可见像素，随后释放 grab 再编码 PNG；鼠标指针不进入 XGetImage。PNG 经
Host artifact side channel 返回，不进入 NDJSON，也不暴露本机路径。provenance 记录 fresh
snapshot/node、应用 PID、目标/顶层/root 窗口、bounds/root size、遮挡检查、same-EUID 与场景稳定性。
截图是 read-only 但输出敏感，且 run-scoped ArtifactRef 不能进入 durable checkpoint。该动作不会
执行 OCR；OCR 只能由 descriptor 显式安排为下一步，任何语义失败也不会自动截图。

## 会话和后端报告

驱动启动时只选择一个后端。Linux 上优先加载可选的 PyGObject `Atspi 2.0`
typelib；该 typelib 缺失或无法初始化时，再使用 PyGObject `Gio 2.0` 直接调用
AT-SPI D-Bus wire 接口。Gio 后备通过当前进程的 session bus 调用
`org.a11y.Bus.GetAddress`，只支持应用枚举和只读快照，全部写动作均返回
`DRIVER.ACTION_UNSUPPORTED`。两种后端都不可用时，能力清单协商仍然成功，而真正的
动作返回 `DRIVER.UNAVAILABLE`。任何失败都不会静默改用坐标输入；Gio 后备也不会
以 no-op 形式伪装写动作成功。
默认后端仅在当前进程环境明确给出 KDE 桌面、`XDG_SESSION_TYPE=x11` 和非空
`DISPLAY` 时启用；环境缺失、Wayland 或其他桌面 profile 均失败关闭。

观察结果会报告实际 backend 和会话证据，包括可获得的
`XDG_SESSION_TYPE`、`XDG_CURRENT_DESKTOP`、`DISPLAY` 与
`DBUS_SESSION_BUS_ADDRESS` 状态。环境变量只是诊断证据，不构成 KDE/X11 已经通过
资格验证的证明。尤其不能把 `DISPLAY` 存在等同于纯 X11 会话，也不能把 XWayland
窗口宣称为完整的 Wayland 支持。

真实后端分别以 `pygobject_atspi` 和 `gio_atspi` 标识；跨平台契约测试使用注入的
fake backend。后端抽象隔离了原生对象，因此模拟测试不会 import PyGObject，也不会
要求图形会话。生产代码只读取当前进程环境，不扫描 `/proc`、不猜测其他会话，也不
连接其他用户的 session bus。

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
输入注入。受保护文本不读取、不回显，`set_text` 和 `type_text` 对此类节点失败关闭。原生对象不会
跨越 NDJSON 边界。状态集合还包括 `checked`、`expandable`、`expanded`、
`selectable` 和 `selected`；无法可靠读取时保留 `null`，不得由 role 或动作名猜测。

`toggle`、`expand` 和 `collapse` 当前只对 PyGObject backend 中的 GTK3 应用公开。
这是显式白名单而非名称猜测：check/toggle 控件必须同时有可观察的 `checked` 状态和
`Action.get_action_name(index) == "click"`；expander 必须有 `expandable=true`、
可观察的 `expanded` 状态和 exact `"activate"`。驱动不使用 localized name 或
description，不 trim、不改大小写，也不接受别名；exact match 缺失或重复时失败关闭。
snapshot provenance 与动作结果均记录 `native_action_name`。其他 toolkit 必须独立资格
验证后才能增加映射。

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

## 显式键盘后备

`desktop.linux_atspi.type_text@1` 是独立动作，不是 `set_text`/`invoke` 的错误处理分支。
调用方必须提供原 snapshot 的 target、原 locator 与普通文本；驱动以原预算 fresh
capture，重新精确 resolve，比较原生身份和语义指纹，并检查普通 text/entry role、
enabled、visible、showing、focusable、editable、sensitive、protected 与进程 ID。任一证据
缺失都在聚焦前失败关闭。Wayland、其他桌面或缺少 `DISPLAY` 返回 unavailable。

文本限制为 1–1024 个 Unicode 字符、最多 4096 UTF-8 字节；除换行外拒绝控制字符，
不支持密码和 secret。helper 是固定路径单次 C++ 进程，非敏感 PID 与单调截止时间走
严格 argv，文本只走 stdin；它核对 XTEST 与焦点窗口祖先 `_NET_WM_PID`。现有键盘映射
无法表达的码点分别占用不同空闲非 modifier keycode，全部映射一次性提交并同步后才
派发，结束时恢复映射。首个合成事件后任一错误/timeout 都映射为非 retryable 的
`DRIVER.UNKNOWN_EFFECT`。成功只表示事件提交，调用方仍须 fresh snapshot 验证文本。
对于 `pointer_click`，若 XTEST 尚未提交但前置聚焦已经改变上下文，错误同样不可重试并标为
contextual effect；只有在聚焦前失败时才能声明 `not_applied`。

驱动一旦进入原生写接口，就使当前公开快照失效。原生接口报错或截止时间在派发后
耗尽时，驱动返回 `DRIVER.UNKNOWN_EFFECT`，不得自动重放。成功响应只证明 AT-SPI
调用返回成功，不证明业务后置条件成立；调用方必须获取新快照验证结果。
`toggle` 是非幂等动作，即使当前 `checked=true` 也会派发并反转状态。`expand` 和
`collapse` 则在完成相同的重新抓树、精确定位、原生身份与语义指纹验证后检查 fresh
`expanded`；若已达到目标态，返回 `dispatched=false, no_op=true`，否则才进入相同的
派发与 `UNKNOWN_EFFECT` 边界。

## NDJSON、截止时间与资源限制

worker 的 stdin/stdout 使用 UTF-8 NDJSON，一行只能包含一个 JSON 对象。stdout
只承载协议帧，诊断写入 stderr。请求上限为 1 MiB，响应必须低于宿主的 8 MiB
单帧上限；超限会返回结构化错误而不是输出部分 JSON。非法 UTF-8、非法 JSON 和
超大请求不会破坏随后合法帧的解析。

Gio 的 `Accessible.GetChildren` 在 D-Bus 返回完整 `a(so)` 后才由绑定解包，因此
`max_nodes` 不能限制单次 wire 响应本身。驱动另设最多 5000 个 child reference 的
单次硬上限，并在解包后立即以 `DRIVER.OUTPUT_TOO_LARGE` 拒绝超限结果，不再复制或
遍历；若需要在传输前限制 fan-out，后续版本必须改用可分页的后端接口。

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

发行版需要提供 Python 3、PyGObject，以及当前用户图形会话可访问的 session bus 和
AT-SPI bus。完整语义后端还需要 `Atspi 2.0` typelib；缺少该 typelib 时，Gio 后备
仍可提供只读能力。必要条件均不可用时，应保守返回 `DRIVER.UNAVAILABLE`。当前实现
不以 root 运行，也不连接其他用户的 D-Bus session。

跨平台测试通过 fake backend 验证 manifest、会话报告、快照归一化、精确与多义
定位、revision/stale、截断保护、七种写动作、deadline 和 NDJSON 帧限制。Linux
真机 smoke 仅在依赖与桌面确实可用时枚举应用并抓取有界快照；无 GUI、无 Gio 或无
AT-SPI bus 时保守跳过或确认 `DRIVER.UNAVAILABLE`，不会把测试环境缺失误报为驱动
成功。测试辅助可以从当前用户的 `kwin_x11` 进程恢复遗漏的 KDE/X11 环境变量，但这条
逻辑不进入生产驱动。若某次运行中 System Settings 未注册进 AT-SPI registry，测试会
明确报告当前 Qt AT-SPI bridge 不可用并跳过，不把进程启动成功误作可访问性支持成功；
当前仓库记录的资格结果里，System Settings 已在 qualifier 的私有 bus 中完成精确 PID
注册和只读快照验证。
仓库还提供自有 GTK3 fixture；当系统安装 `Atspi 2.0` typelib 时，它通过正式进程驱动
真实验证 `snapshot/find`、`Component.grab_focus`、`EditableText.set_text_contents` 与
`Action.do_action`；动作后还会重新抓取快照核对文本、checked 和 expanded 状态。当前
会话的窗口管理器没有通过 AT-SPI 回报 fixture 的焦点状态，所以 focus 仅验证原生调用
被接受。测试临时 overlay 可通过
`AI_AUTO_DESKTOP_TEST_ATSPI_TYPELIB_PATH` 显式传入，不会修改系统安装。
仓库同时提供自有 Qt 5 Widgets C++ fixture。测试会在本机按需编译它，并显式设置
`QT_LINUX_ACCESSIBILITY_ALWAYS_ON=1`/`QT_ACCESSIBILITY=1` 后检查是否注册到 AT-SPI。
本次 Debian 12、Plasma 5.27.5、Qt 5.15.8、X11 环境中，当前长期 AT-SPI registry
一度无法接收新 Qt application；测试因此改为在同一真实 X11 display 上启动隔离的
session/accessibility bus，现已真实通过 Qt Widgets 的 snapshot/find/focus/set_text/invoke
及动作后重新观察。GTK3 的真实语义动作链路同样通过。另以隔离 AT-SPI bus 的已解锁
KDE/X11 display，以及私有 Xvfb + 私有 AT-SPI bus，分别验证 GTK3/Qt5 `type_text` 的
真实 XTEST UTF-8 输入和 fresh snapshot 后置条件；未使用 OCR、`xdotool`、剪贴板或
坐标点击。锁屏时 XTEST 请求可能被会话吞掉，因此成功响应仍不能替代后置条件。
生产动作当前不读取登录管理器锁定状态；锁屏/登录界面明确不受支持，调用方必须在
解锁的用户会话执行，并把返回字段 `submitted=true` 理解为“X server 已接受事件请求”，
而不是“应用已接收文本”。
真实 KDE 应用验证另使用发行版 KCalc 22.12.3，而非仓库 fixture。测试在禁用 TCP、使用
一次性 Xauthority 的私有 Xvfb/KWin，以及私有 session/AT-SPI bus 和临时 HOME/XDG 中
启动独占 KCalc PID；每次从未截断的 fresh snapshot 精确定位 `1`、`+`、`2`、`=` 四个
按钮。计算分别走 `Action.do_action` 的 exact `Press` 与显式 `pointer_click`：后者由 fresh
bounds 推导中心点，经 AT-SPI subtree hit-test、KWin 焦点与 X11 点下窗口 PID 复核后用
XTEST 派发。两条路径最后都从 fresh snapshot 唯一读取同一显示节点 `3`。这证明了一个
真实 KDE/Qt Widgets 应用的受控写动作闭环，但不外推为任意 KDE 应用、页面或动作均已通过；
整个 case 不使用 OCR、截图或用户配置目录。
代码中为 Qt 5 Widgets 保留了保守的已观测映射：按钮只有在 exact canonical `Press`
唯一存在时才公开 `invoke`；由于 Qt 5 bridge 不导出 `AccessibleId`，写前身份验证要求
bus/object path、toolkit/version 与进程 ID 全部一致，并继续比较语义指纹。该映射只有在
fixture 真正注册后才会执行，也不能替代真实 KDE 应用矩阵的资格验证。

“KDE/X11 已资格验证”还需要在固定发行版、Plasma、Xorg 与 Qt 版本上，以 Qt Widgets、
QML、Dolphin、System Settings、Konsole、对话框、多窗口、虚拟列表和多显示器/DPI
样例分别记录语义完整度、错误目标率、动作覆盖率、延迟及挂起情况。fake backend 测试
和一次成功枚举都不构成这一发布声明。
