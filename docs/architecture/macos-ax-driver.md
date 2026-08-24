# macOS Accessibility（AX）进程驱动

> 状态：跨平台可审查骨架，2026-08-25。Python 契约核心、helper 完整性边界和真实
> Accessibility API helper 源码已经落盘；当前开发主机不是 macOS，因此尚未完成 Swift
> 编译、TCC 授权或真实应用资格验证。本文只描述设计与受测试的纯逻辑保证，不把未执行的
> macOS 验证写成已完成事实。

## 进程与信任边界

公开能力名是 `desktop.macos_ax`，动作是 `list_apps`、`snapshot`、`find`、`focus`、
`invoke`、`set_value` 和 `type_text`。manifest 只声明 `runtime.platforms: [macos]`，入口为
`./run.sh`。观察动作要求 `desktop.observe`；四个写动作还要求 `desktop.input`。

实现分为两层：

```text
runtime
  │ public NDJSON: full action IDs, deadlines, structured DRIVER.* errors
  ▼
Python worker
  │ private NDJSON: bounded operation args, opaque native tokens
  ▼
integrity-checked MacOSAXHelper.app
  │ ApplicationServices Accessibility API
  ▼
one exactly selected running application PID
```

Python worker 承担 manifest、输入验证、snapshot revision、精确 locator、stale/truncated
规则和 effect 归一化。这部分不 import macOS framework，可在任意平台通过 injected fake
backend 测试。生产默认后端只允许位于
`MacOSAXHelper.app/Contents/MacOS/MacOSAXHelper` 的可执行文件，并在启动前执行
`codesign --verify --strict`，同时核对 `Info.plist` 的固定 bundle ID、可执行文件名和
package type。该检查是签名完整性检查，不是来源认证：当前实现不 pin Team ID、证书或
发布者。显式配置的自定义 helper 一律标记 `custom_untrusted` /
`source_authenticated=false`，部署者必须在本驱动之外建立来源信任。原生 helper 使用
AppKit 枚举 `NSRunningApplication`，使用
ApplicationServices 的 `AXUIElementCreateApplication`、属性 API 和动作 API；Python 没有
PyObjC 或 AppleScript 路径，键盘输入仅存在于显式 `type_text` 动作，不是 `set_value` 的
自动 fallback。

原生 `AXUIElement` 仅保存在 Swift 进程的短期 token store 中，绝不出现在公开 NDJSON。
每次 snapshot 开启新 token generation，只保留当前和上一 generation，以便写前重抓后比较
旧/新对象；更早 token 失效。Python 退出时终止 helper。非 macOS、helper 缺失、bundle
结构错误、签名验证失败、协议握手不兼容或 TCC 未授权都会明确失败，不会静默降级。

## 应用枚举与快照

`list_apps` 返回当前 Aqua session 中可见于 `NSWorkspace` 的运行中进程元数据以及
`accessibility_trusted`。生产后端还报告 `helper_security`，区分完整性验证与来源认证。枚举
本身不读取 AX 树，因此可用于诊断权限状态。`snapshot` 才要求 helper 已获得 Accessibility
权限。应用记录不输出 executable path；多义选择错误只返回匹配数量，不回传其他应用元数据。

应用选择器至少包含 `process_id`、`bundle_id` 或 `name` 之一。所有给出的字段都精确匹配；
零匹配返回 `DRIVER.NOT_FOUND`，多匹配返回 `DRIVER.AMBIGUOUS`，绝不默认取第一项。随后只
调用 `AXUIElementCreateApplication(selectedPID)`，不创建 system-wide AX root。

快照以 BFS 读取 `AXChildren`，同时受 `max_depth`（最大 128）和 `max_nodes`（最大 5000）
限制。helper 先调用 `AXUIElementGetAttributeValueCount`，再用
`AXUIElementCopyAttributeValues` 只取剩余预算内的 children。无法证明剩余子树为空时设置
`truncated: true`。外层结构稳定为：

```json
{
  "snapshot_id": "<worker-generation>:<revision>",
  "revision": 1,
  "backend": "macos_ax_swift_helper",
  "app": {},
  "nodes": [],
  "truncated": false
}
```

节点是带 `parent_id` 的扁平列表，字段包括 `role`、`subrole`、`name`、
`description`、`value`、`states`、`bounds`、`actions` 和 `provenance`。bounds 使用 macOS
全局屏幕 point 坐标，只作为观察信息，不能用于本驱动的输入回退。helper 只把可预检的动作
声明到 `actions`：`AXFocused` 可写对应 `focus`，`AXPress` 存在对应 `invoke`，`AXValue`
可写对应 `set_value`；非受保护、enabled、可聚焦且 role 为 `AXTextField`、`AXTextArea` 或
`AXComboBox` 的节点对应 `type_text`。secure text 的值始终是 null，标记 `protected` 和
`value_redacted`，且不声明 `set_value` 或 `type_text`。

## 精确定位、revision 与 stale

locator 可以组合 role、subrole、name、description、value、AX identifier、states 和 actions。
当前版本只支持区分大小写的 exact matching。`match` 省略时等同 `exact`；只提供
`match`、空 states/actions、未知字段或未知动作均返回 `DRIVER.INVALID_REQUEST`。零匹配返回
`DRIVER.NOT_FOUND`；多匹配返回 `DRIVER.AMBIGUOUS` 和匹配数量，不回传候选节点内容。截断快照不能
证明全树唯一，因此 `find` 返回 `DRIVER.SNAPSHOT_TRUNCATED`。

worker 同时只保留一个公开 current revision。再次 snapshot、写前内部重抓或完成一次原生写
都会使旧 target 失效。`find` 返回的 target 包含 snapshot ID、revision 和 node ID，但这些
都不是跨 worker 或持久化的 element handle。

每个写动作必须同时提供 target 和原 locator，派发流程如下：

1. 校验 target 属于 current revision，且与旧快照中 locator 的唯一结果一致。
2. 沿用原快照的 depth/node budget 对同一 app selector 重抓。
3. 在新快照中再次精确解析 locator；未找到或多义归一为 `DRIVER.STALE_SNAPSHOT`。
4. 拒绝任何旧或新快照截断情况。
5. 由 helper 用 `CFEqual` 比较两个未出进程的 `AXUIElement`，并比较 Python 语义指纹。
6. 再次检查 action 支持及 protected 状态，才进入原生派发边界。

任何身份无法验证、元素被替换、语义变化或 locator 漂移都在未派发动作时失败关闭。

## 动作、效果与错误

`focus` 先用 `AXUIElementIsAttributeSettable` 预检，再写 `AXFocused=true`；`set_value`
同样预检后写 `AXValue`；`invoke` 先从 `AXUIElementCopyActionNames` 确认 `AXPress`，再调用
`AXUIElementPerformAction`。`type_text` 是独立显式动作：Python 先拒绝空文本、NUL/C0/C1
控制字符、孤立 surrogate、超过 1024 Unicode 标量或 2048 UTF-16 code units 的输入；Swift
重复同一边界校验，
检查非 secure 文本 role、enabled、AX 可聚焦和目标 PID 为当前前台应用，设置并回读
`AXFocused=true` 后，才以 `CGEventKeyboardSetUnicodeString` 按最多 20 个 UTF-16 code units
且不拆 surrogate pair 的块构造 key-down/key-up，并通过 `postToPid` 投递；每块前都再次确认
目标仍有焦点且应用仍在前台。它不会激活后台应用，不会模拟 pointer，也不会被 `set_value`
隐式调用。
`type_text` 只需要 Accessibility，不需要 Screen Recording。完整的换行终止 helper 请求帧
写入 pipe 只表示跨过请求传输边界，并不表示键盘已经派发；部分写入或写入失败仍是
`not_applied`。
该动作不是 secret 输入通道、快捷键或粘贴接口，明文会经过公开和私有 NDJSON 边界；调用方
不得用它输入密码或其他 secret。
在改变焦点和发布首个事件前，helper 使用 Carbon/HIToolbox 的
`IsSecureEventInputEnabled()` 检查系统 Secure Event Input；开启时 fail closed，返回
`phase=secure_event_input_preflight`、`keyboard_dispatch_started=false`、`effect=not_applied`。

manifest 将 `focus`、`set_value` 和 `type_text` 标为 `contextual`，将 `invoke` 标为
`non_idempotent`。写动作的完整换行帧写入 helper pipe 后，Python 才视为已派发：原生
`DRIVER.ACTION_FAILED`、派发后 timeout 或未预期异常都归一为
`DRIVER.UNKNOWN_EFFECT(effect=unknown)`，并使 current snapshot 失效。完整写入写请求后的 helper
timeout、EOF、输出超限或协议错误同样按这一规则归一；这些致命通道错误会立即 kill helper，
该 backend 不可复用。`type_text` 不把完整 helper 请求当成键盘派发：helper 只在首个
`keyDown.postToPid` 前发送进度帧 `keyboard_dispatch_started=true`，Python 观察到该帧后才会把
失败或 timeout 归一为 `DRIVER.UNKNOWN_EFFECT` 并强制 kill helper。结构化 preflight 错误保持
`not_applied`；若 AX focus 已改变但未发送按键，则报告 `focus_changed=true` 和
`effect=contextual`；helper 在确认焦点后先发独立进度帧，所以此阶段的 timeout 也不声称文本
可能输入。成功结果使用 `submitted=true`，只说明 CGEvent 调用已
提交，不证明目标应用已经处理。调用方不得自动重试，而应获取新快照验证业务后置条件。

稳定错误码包括：

- 参数/协议：`DRIVER.INVALID_REQUEST`、`PROTOCOL.INVALID_REQUEST`、
  `PROTOCOL.PARSE_ERROR`、`PROTOCOL.INVALID_ENCODING`、
  `PROTOCOL.REQUEST_TOO_LARGE`、`PROTOCOL.ACTION_NOT_FOUND`；
- 环境/资源：`DRIVER.UNAVAILABLE`、`DRIVER.TIMEOUT`、
  `DRIVER.OUTPUT_TOO_LARGE`、`DRIVER.ACTION_FAILED`；
- 解析/一致性：`DRIVER.NOT_FOUND`、`DRIVER.AMBIGUOUS`、
  `DRIVER.STALE_SNAPSHOT`、`DRIVER.SNAPSHOT_TRUNCATED`；
- 写边界：`DRIVER.ACTION_UNSUPPORTED`、`DRIVER.PROTECTED_ELEMENT`、
  `DRIVER.UNKNOWN_EFFECT`。

## NDJSON 与截止时间

公开 worker 的 stdin/stdout 是 UTF-8 NDJSON，stdout 只输出协议帧，诊断写入 stderr。请求
上限 1 MiB，响应必须低于宿主 8 MiB 帧限制；非法 UTF-8、非法 JSON 和超大帧不会破坏后续
合法请求。公开调用的 `deadline_ms` 是 Unix epoch 毫秒绝对时间，Python 转换为 monotonic
deadline，再把剩余预算转发给 helper。

helper 自己以 64 KiB chunk 读取 stdin，超过 1 MiB 后丢弃到下一个换行，不使用可能无界
分配的 `readLine()`；stdout 编码后也执行硬上限检查，超限只返回有界的
`DRIVER.OUTPUT_TOO_LARGE`。helper 会在遍历、解析和原生动作前检查 deadline，并对每个 AX element 调用
`AXUIElementSetMessagingTimeout`，timeout 不超过两秒和当前请求剩余时间中的较小值。不过同步
AX API 仍不能由 Python 安全抢占，因此宿主进程 timeout/回收是最终硬边界。

## 已验证与待验证

跨平台 fake backend 测试覆盖 manifest schema、归一化快照、精确/多义/未找到、revision、
stale identity、truncated fail-closed、protected value、四个写动作、输入边界、无自动 fallback、UNKNOWN_EFFECT、deadline、
非 macOS `DRIVER.UNAVAILABLE` 和 NDJSON 帧恢复。静态测试还检查 helper 源码确实使用 AX API、
限制到应用 PID、预检写能力且构建脚本执行签名验证。

这些测试不等于真实 AX 资格验证。发布前至少还应在 Intel 与 Apple Silicon、固定 macOS
版本和交互式登录会话中完成：Swift 编译、ad-hoc/Developer ID 签名、首次授权和重签后的
TCC 行为、自有 AppKit fixture 的 snapshot/find/focus/set_value/invoke 闭环，以及真机 kit
已经实现但尚待在真实 Mac 执行的显式 `type_text` Unicode/前台焦点/secure text 拒绝闭环、应用退出与 PID
复用、对话框/多窗口、虚拟列表、多显示器、Retina 坐标，以及无响应/拒绝/受保护界面。完成
这些证据之前，正式状态仍是“严格骨架，未资格验证”。
