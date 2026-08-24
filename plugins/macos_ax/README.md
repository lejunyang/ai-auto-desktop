# macOS AX 进程驱动

`desktop.macos_ax` 是 macOS 原生 Accessibility API（AX）能力的进程驱动。它提供
`list_apps`、`snapshot`、`find`、`focus`、`invoke`、`set_value` 和显式键盘输入
`type_text` 七个 v1 动作。

当前状态必须准确理解为：**跨平台协议核心和 native helper 源码已实现，但尚未在本仓库的
macOS CI/真机上完成编译及资格验证**。Linux 等非 macOS 主机只运行 fake backend 契约测试；
这不证明真实 AX 调用可用。未构建 helper、签名无效或未授予 Accessibility 权限时，生产
路径会明确返回 `DRIVER.UNAVAILABLE`，不会使用伪实现、AppleScript、坐标点击或键鼠注入兜底。

## 组成

- `macos_ax_driver.py`：公开的 UTF-8 NDJSON worker、能力 manifest 和语言无关的快照/定位/
  stale/effect 契约。
- `swift/MacOSAXHelper.swift`：唯一的生产 AX 执行面。原生 `AXUIElement` 只保存在 helper
  进程内，通过短期 opaque token 与 Python 适配层关联。
- `build.sh`：使用 `xcrun swiftc` 构建 `.app`，随后签名并严格验证。
- `run.sh`：启动 Python worker。macOS 插件不提供 `run.cmd`。

## 构建和运行

在 macOS 13+、已安装 Xcode Command Line Tools 的终端中运行：

```sh
plugins/macos_ax/build.sh
plugins/macos_ax/run.sh
```

默认构建结果是：

```text
plugins/macos_ax/.build/MacOSAXHelper.app/Contents/MacOS/MacOSAXHelper
```

`build.sh` 默认做 ad-hoc 签名。稳定部署应使用固定签名身份：

```sh
MACOS_AX_CODESIGN_IDENTITY='Developer ID Application: Example' \
  plugins/macos_ax/build.sh
```

也可以设置 `AI_AUTO_DESKTOP_MACOS_AX_BUILD_DIR` 改变构建目录，或在运行 worker 前设置
`AI_AUTO_DESKTOP_MACOS_AX_HELPER` 指向另一个 `MacOSAXHelper.app/Contents/MacOS/
MacOSAXHelper`。无论使用哪个路径，Python 都要求 helper 位于该固定 app bundle 结构中，
执行 `codesign --verify --strict`，并校验 `Info.plist` 中固定的 bundle ID、可执行文件名与
package type；不接受裸 Swift 可执行文件。这里的签名检查只证明 bundle 相对其签名未被
修改，**不认证发布者或文件来源**。当前实现没有 pin Developer ID/Team ID。显式指定的
helper 始终报告 `helper_security.source=custom_untrusted` 和
`source_authenticated=false`，即使完整性检查通过也必须由部署者另行建立来源信任。

首次使用需要在“系统设置 → 隐私与安全性 → 辅助功能”中授权
`MacOSAXHelper.app`。helper 只调用 `AXIsProcessTrusted()` 做静默检查，不主动弹出 TCC
授权框。重编译 ad-hoc 签名应用可能导致授权身份变化；固定 Developer ID 签名和固定路径更
适合长期部署。

## NDJSON 示例

协议请求使用完整 action ID，一行一个 JSON 对象：

```json
{"type":"manifest","id":"m1"}
{"type":"invoke","id":"a1","action":"desktop.macos_ax.list_apps@1","args":{}}
```

获取快照时必须使用精确应用选择器：

```json
{"type":"invoke","id":"s1","action":"desktop.macos_ax.snapshot@1","args":{"app":{"bundle_id":"com.example.Editor"},"max_depth":32,"max_nodes":1000}}
```

写动作必须携带同一当前快照的 `target` 和最初的精确 `locator`。driver 会按原预算重抓
AX 树、重新定位，并通过 helper 内的 `CFEqual` 比较原生对象身份；任一步无法证明一致时都在
派发前失败。`invoke` 可能非幂等，返回 `DRIVER.UNKNOWN_EFFECT` 时不得自动重试。

`desktop.macos_ax.type_text@1` 必须由调用方显式选择；`set_value` 失败时绝不会自动
fallback。它接收 `target`、`locator` 和 `text`，只允许 1–1024 个 Unicode 标量且不超过
2048 个 UTF-16 code units 的
非控制字符文本。helper 在目标应用保持前台时先设置并确认 `AXFocused`，随后通过
`CGEventKeyboardSetUnicodeString` 向目标 PID 提交 Unicode 键盘事件。成功仅表示调用已提交，
不证明应用已经接收或处理文本。示例（`target` 来自当前
快照的 `find`）：

```json
{"type":"invoke","id":"t1","action":"desktop.macos_ax.type_text@1","args":{"target":{"snapshot_id":"...","revision":1,"node_id":"n3"},"locator":{"identifier":"title"},"text":"你好 macOS"}}
```

## 明确边界

- 不支持模糊、contains 或正则定位；所有字段区分大小写并精确相等。
- 截断快照不能用于 `find` 或写动作。
- 密码/secure text 的值不读取、不回传，也不允许 `set_value` 或 `type_text`。
- 不调用 `AXUIElementCreateSystemWide`，每棵树都限定在一个精确选中的 PID。
- `type_text` 需要 Accessibility 权限，不需要 Screen Recording；目标应用必须保持前台，
  不会自动激活应用。驱动不截图、不调用 AppleScript、不注入 pointer 事件。
- `type_text` 只对 `AXTextField`、`AXTextArea`、`AXComboBox` 暴露，拒绝空文本、NUL、
  C0/C1 控制字符、孤立 surrogate 和超过上限的输入；换行或 Tab 应使用未来单独的按键动作。
- helper 在首个 `keyDown.postToPid` 前检查系统 `IsSecureEventInputEnabled()`；启用时 fail closed，
  明确返回 `keyboard_dispatch_started=false` / `not_applied`。
- 完整 NDJSON 请求本身不代表键盘已派发。helper 仅在首个 key-down 前发出
  `keyboard_dispatch_started=true` marker；只有越过该边界后的失败或 timeout 才归一为
  `DRIVER.UNKNOWN_EFFECT` 并 kill helper。焦点已经改变但尚未发键时报告 `focus_changed=true` /
  `effect=contextual`；helper 在确认焦点后先发送独立进度帧，因此随后 timeout 也不会声称文本
  可能已输入。
- `type_text` 不是密码输入、快捷键或粘贴接口：文本会经过进程间 NDJSON，调用方不得把
  secret 作为普通文本传入。
- AX API 是同步 API；helper 为每个 element 设置不超过请求剩余时间的 messaging timeout，
  外层进程超时仍是最终硬边界。
- helper 私有 stdin 和 stdout 都有硬帧上限。响应 timeout、EOF、协议错误或超限会立即
  kill helper，通道不能复用。一般 AX 写请求完整写入后的通道失败仍可能是 unknown；
  `type_text` 以独立的 `keyboard_dispatch_started` marker 判断文本效果。
- 当前源码没有经过 macOS 真机构建、TCC、Intel/Apple Silicon、多显示器或第三方应用验证，
  因此不能据此声明平台资格已完成。

完整契约和后续真机验收项见 `docs/architecture/macos-ax-driver.md`。
