# Windows 用户界面自动化（UIA）进程驱动

> 状态：首个纵向切片，2026-08-24。该能力仅支持 Windows，并通过可选的
> `comtypes` 绑定使用 `UIAutomationClient`。

## 契约与边界

该能力提供方对外声明 `metadata.name: desktop.windows_uia`，并提供以下 v1
动作：`list_windows`、`snapshot`、`find`、`focus`、`invoke`、
`set_value`、显式键盘后备 `type_text` 和显式鼠标输入 `pointer_click`。工作流中的 `uses` 值由能力清单标识、动作键和契约主版本号
组成，例如 `desktop.windows_uia.snapshot@1`。能力清单的
`runtime.platforms` 仅声明 `windows`。

工作进程以“一行一个 JSON 对象”的方式读写数据。输出会转义为 ASCII 安全的线上表示，
因此格式错误的 UTF-16 代理项值不会导致进程终止。stdout 只承载协议输出；有界诊断信息
写入 stderr。请求上限为 1 MiB，响应必须低于宿主进程的 8 MiB 帧上限。
`deadline_ms` 是 Unix epoch 毫秒绝对时间。工作进程会将它转换一次为单调时钟
截止时间，并在枚举窗口、遍历树、解析定位器以及每次原生写操作之前立即检查。
Python 无法安全抢占单次 COM 调用，因此宿主进程的进程超时仍是最终的硬停止边界。

后端依赖是可选的。在非 Windows 系统上，或 `comtypes` /
`UIAutomationCore` 无法初始化时，能力清单协商仍可成功；需要实际访问 UIA 后端的
调用会返回结构化的 `DRIVER.UNAVAILABLE`。这样既能让使用模拟后端的契约测试跨平台
运行，又不会对该能力的平台适用范围作出不实声明。

当前切片仅支持 `type_text` 的有界 Unicode 键盘注入和 `pointer_click` 的显式鼠标注入，
不支持快捷键、组合键、截图、OCR 或可点击点回退机制，也不会隐式改用其他定位器。
`set_value` 只使用 `ValuePattern.SetValue`，失败时绝不会自动退化为 `type_text`；
其他动作也绝不会自动退化为 `pointer_click`。

## 归一化观察结果

每个 `snapshot` 结果都具有以下稳定的外层结构：

```json
{
  "snapshot_id": "<worker-generation>:<revision>",
  "revision": 1,
  "app": {},
  "window": {},
  "nodes": [],
  "truncated": false
}
```

节点采用带父节点引用的扁平列表，并包含 `node_id`、`parent_id`、`role`、
`name`、`value`、`states`、`bounds`、`actions` 和 `provenance`。UIA
提供边界信息时，`bounds` 使用物理屏幕像素。密码值不会被读取。原生 COM 元素绝不
跨越进程边界。执行写操作前，真实 UIA 后端会使用 `CompareElements` 确认重新解析得到的
COM 元素仍是原来的原生目标；RuntimeId 和语义指纹也参与目标替换检测，但它们不是持久、
可重放的句柄。

`snapshot_id`、`revision` 和节点 ID 仅在工作进程的当前修订版本内有效。再次获取
快照会使先前的节点引用失效；驱动重启则会改变 `snapshot_id` 中的世代标识部分。

## 定位器与写操作语义

定位器（`locator`）是针对 role、name、value、automation ID、class name、framework ID、
states 和所支持动作的结构化谓词。当前 v0 切片只支持区分大小写的精确字符串比较。
匹配数为零时，解析器返回 `DRIVER.NOT_FOUND`；匹配数大于一时，则返回
`DRIVER.AMBIGUOUS` 以及有界的候选摘要，绝不会直接选择第一个候选项。

`find` 返回一个外层包含 `target` 和匹配节点 `node` 的结果，其中 `target` 是
限定在当前快照内的目标：

```json
{
  "target": {
    "snapshot_id": "...",
    "revision": 1,
    "node_id": "n12"
  },
  "node": {}
}
```

每个写入动作都必须同时携带该 `target` 和原始定位器。派发前，工作进程会验证
目标属于当前快照，以原快照的深度和节点数上限重新抓取 UIA 树，再次解析原始
定位器，并比较原生身份与语义指纹。目标消失、变为多义、无法验证或已被替换时，会在
不调用原生写模式的前提下返回 `DRIVER.STALE_SNAPSHOT`。原快照已被截断，或派发前的
截断快照仍解析出唯一目标时，则返回 `DRIVER.SNAPSHOT_TRUNCATED`；派发前解析已经变为
未找到或多义时，会先归一化为 `DRIVER.STALE_SNAPSHOT`。这些情况都不会执行写操作。
原生写操作一旦进入后端派发边界，即使后端报告失败，也会使当前快照失效，因为工作进程
无法证明派发之后 UI 树仍未改变。当前成功响应不会在派发后重新观察 UI，也不验证
action-specific postcondition；如需确认界面结果，调用方必须获取新快照。

效果分类采取保守策略：`invoke` 与 `pointer_click` 的 `effect.default_class` 为 `non_idempotent`；
`focus`、`set_value` 和 `type_text` 的 `effect.default_class` 为 `contextual`。错误使用稳定的
`DRIVER.*` 错误码，并携带有界诊断细节；部分定位失败细节会回显原始定位器或候选节点
摘要，因此这里不提供通用的 secret-redaction 保证。原生调用前可以确认未生效的失败保持
`not_applied`；进入后端派发边界后若发生 `DRIVER.ACTION_FAILED`、
`DRIVER.TIMEOUT` 或未预期异常，则转换为 `DRIVER.UNKNOWN_EFFECT`，不得盲目重放。

### 显式 pointer_click

`desktop.windows_uia.pointer_click@1` 接收 `target`、`locator`，可选 `button` 与
`position`；v0 仅接受 `button=left`、`position=center`。它不接受裸 `x/y`，调用方也不能
借 locator 直接要求绝对屏幕点。驱动先执行与其他写动作一致的 fresh capture、唯一重解析、
`CompareElements` 原生身份与语义指纹校验，然后要求目标在 fresh snapshot 中具备可证明的
PID 归属、`enabled=true`、`offscreen=false`，并且 `bounds` 必须存在且为正面积。

为提高 discoverability，snapshot 只会在满足 `enabled && !offscreen && bounds.width > 0 &&
bounds.height > 0 && process_id > 0` 时把 `pointer_click` 广告到节点 `actions`。但这只是候选
资格提示，不是 dispatch 证明；真正执行前仍要再次验证目标 PID、fresh snapshot 顶层窗口
`HWND/PID`、以及当前 foreground `HWND/PID` 与目标完全一致。

鼠标注入通过一批 Win32 `SendInput` mouse `INPUT` 完成，固定发送
`MOVE + LEFTDOWN + LEFTUP`。坐标按虚拟桌面边界归一化到 `0..65535`，始终携带
`MOUSEEVENTF_ABSOLUTE | MOUSEEVENTF_VIRTUALDESK`，因此支持包含负坐标的多显示器布局，
并在虚拟桌面端点精确钳制到 `0` 或 `65535`。该 action 不会自动执行额外聚焦、不会回退到
UIA InvokePattern，也不会在失败后自动重试。

effect boundary 由 `SendInput` 返回的 accepted count 决定。0 个事件表示 `not_applied`；
一旦至少 1 个事件已被接受，后续异常、部分提交或 post-dispatch timeout 都必须归一化为
不可重试的 `DRIVER.UNKNOWN_EFFECT`。成功响应使用 `submitted=true`，只说明该批鼠标事件已进入
Windows 输入流，不能等价为“按钮已点击”或“界面已变化”，调用方仍须重新观察并验证
postcondition。

### 显式 Unicode 键盘后备

`desktop.windows_uia.type_text@1` 接收 `target`、`locator` 和 `text`，要求
`desktop.observe` 与 `desktop.input` 权限，风险为 `input/high`。它完整复用上述 fresh capture、
唯一重解析、`CompareElements` 原生身份、语义指纹和 protected 校验，然后通过 UIA
`SetFocus` 聚焦目标，并确认目标已获得键盘焦点、目标 PID 与前台窗口 PID 一致，且前台 HWND
等于 fresh snapshot 记录的顶层 `window.handle`。驱动以单个 Unicode scalar 为一批调用 Win32
`SendInput(KEYEVENTF_UNICODE)`；每批前重新检查焦点、前台 HWND 和 PID。BMP 字符每批两个
事件，非 BMP 代理对同批发送四个事件；返回结果仅包含 scalar、code unit 与
`events_submitted` 计数，不回显文本。

输入仅限 1 到 1024 个合法 Unicode 字符，拒绝未配对代理项以及 Unicode `C*` 类别的
控制、格式、代理、私用区和未分配码位，所以换行、Tab、ESC 等不会借此动作发送。密码或
UIA `CurrentIsPassword` 标记的 protected 元素始终在发送前拒绝。此能力是文字输入，不是
快捷键或 pointer API。

效果边界以 `SendInput` 返回的已提交 `INPUT` 数为准。聚焦失败、截止时间在发送前耗尽、
异常或返回 0 都是 INPUT pre-dispatch，文本结果为 `effect=not_applied`；此前的 `SetFocus` 是
准备步骤，仍可能已改变焦点，错误细节会标记 `focus_may_have_changed`。从首个事件被提交开始，
后续焦点/HWND/PID 漂移、部分提交、超时或结果无法确认都返回不可重试的
`DRIVER.UNKNOWN_EFFECT`。该 action 的契约
把所有错误声明为 `retryable=false`，禁止运行时自动重放。成功也只证明事件已入 Windows 输入
流，并不证明控件最终值，仍须重新观察并验证 postcondition。

`SendInput` 不能跨越 UIPI 完整性级别：普通进程通常无法输入到管理员窗口，也不能用于
Session 0、UAC secure desktop、登录或锁屏桌面。输入依赖当时的前台/焦点状态，窗口切换或
用户介入可能改变接收目标。逐 scalar 复检只能缩小 TOCTOU 窗口；焦点查询与 `SendInput`
并非原子操作，仍无法彻底消除用户恰在两者之间抢走焦点的竞态。`KEYEVENTF_UNICODE` 避免按
当前键盘布局反查虚拟键，但应用、IME 和控件仍可自行解释或拒绝事件；Windows 对 UIPI 阻止
也不保证提供可区分的错误码。

## 启动与资格验证

在 Windows 上，使用 `pip install .[windows-uia]` 安装可选依赖，然后运行
`plugins\windows_uia\run.cmd`；也可以向 Python 传入包含
`windows_uia_driver.py` 的显式参数列表。`run.sh` 只用于 POSIX 宿主机上的协议测试和
模拟后端契约测试。

Linux/macOS 测试路径会验证能力清单、不可用状态行为、归一化快照、精确/多义/未找到
解析、目标替换后的过期检测、通过模拟后端执行的写入动作语义、截止时间、
结构化错误、Unicode 代理对编码、mouse 虚拟桌面归一化、INPUT 派发边界和输入帧上限；
模拟后端测试不等同于真实 UIA 调用资格验证。Windows-only 原生测试会在自有 Win32 fixture 上
显式执行 `type_text` 与 `pointer_click`，并通过 fresh snapshot 重新观察文本/状态变化；
在非 Windows 环境该测试文件只做 skip，不尝试伪造 Win32 输入行为。提权/UIPI、
secure desktop、输入法与前台焦点竞争仍需单独的平台验收，不能由自有 fixture 的通过外推。
