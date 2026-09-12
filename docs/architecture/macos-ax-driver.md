# macOS Accessibility（AX）Rust 驱动

> 当前实现：`crates/aad-macos-ax` + 代码签名 Swift helper。Rust 负责公开 provider 语义；
> Swift 只保留 TCC/AX/CGEvent 原生边界。

## 进程与信任边界

```text
Rust runtime / CLI / MCP
  │ in-process Provider calls
  ▼
aad-macos-ax (manifest, validation, snapshots, locators, effects)
  │ private bounded NDJSON, opaque native tokens
  ▼
MacOSAXHelper.app (AppKit / ApplicationServices / CGEvent / TCC identity)
```

公开动作是 `list_apps`、`snapshot`、`find`、`focus`、`invoke`、`set_value`、显式
`type_text` 和显式 `pointer_click`。观察需要 `desktop.observe`，写动作还需要
`desktop.input`。仓库不包含 Python adapter，也没有 PyObjC/AppleScript fallback。

Rust adapter 只接受固定 `.app/Contents/MacOS/MacOSAXHelper` 结构，启动前执行
`codesign --verify --strict` 并核对 bundle ID、可执行名与 package type。完整性检查不等于
发布者认证；自定义 helper 明确标为 `custom_untrusted/source_authenticated=false`。
`AI_AUTO_DESKTOP_MACOS_AX_HELPER` 可覆盖 helper 路径，部署者需另行建立来源信任。

## AX snapshot 与写前复核

`list_apps` 只枚举当前 Aqua session 的应用并报告 Accessibility trust。`snapshot` 要求精确
app selector，只从所选 PID 创建 AX root；BFS 遍历受 depth/node 上限约束。protected 值保持
redacted，native `AXUIElement` 只存在 helper 的短期 token store 中。

locator 是区分大小写的 exact matching；零匹配、多匹配和截断快照都失败关闭。写动作必须
携带 current target 与原 locator。Rust adapter 以原预算 fresh capture、重新唯一定位并比较
语义 fingerprint；helper 再用 `CFEqual` 核对新旧 AX 对象。身份变化或无法证明一致时不派发。

`focus`/`set_value` 预检属性可写性，`invoke` 预检 `AXPress`。`type_text` 仅对安全文本角色
开放，先确认 Secure Event Input 未开启、目标已聚焦且应用仍在前台，再分块提交 Unicode
CGEvent。`pointer_click` 从 fresh bounds 计算中心点，要求 AX hit-test 回到同一元素且应用仍
frontmost，再提交 mouse moved/down/up。二者都不接受裸坐标，也不会作为其他动作的隐式
fallback。

helper 在首个键盘或鼠标事件前发送 progress marker。marker 前的失败保持 not-applied 或
contextual；marker 后的 timeout、EOF、协议错误、部分响应和无法确认统一为不可重试的
`DRIVER.UNKNOWN_EFFECT`。成功只证明系统 API 接受提交，调用方仍须 fresh AX snapshot 验证
业务结果。

## 构建和验证

```sh
plugins/macos_ax/build.sh
cargo run --locked -q -p aad-macos-ax --example list_apps
```

macOS CI 构建并签名 helper，再通过 Rust adapter 做握手和应用枚举 smoke。Rust fake-helper
测试在所有平台验证 manifest、snapshot/locator、stale/protected、deadline、progress marker
和 unknown-effect 归一。

`tests/macos/` 另有无 Python 的 Swift 真机套件，覆盖自有 AppKit fixture 的 AXFocused、
AXValue、AXPress、Unicode CGEvent、secure text 拒绝与中心点 pointer 回读。当前 Linux 开发机
不能执行 TCC/Apple framework，因此不能用本机验证替代真实 Mac 资格证据。

## 明确边界

- 首次使用需给 `MacOSAXHelper.app` Accessibility 权限；provider 不主动弹窗。
- 不创建 system-wide AX root，不读取 executable path，不截图，不请求 Screen Recording。
- 不支持锁屏、安全输入、跨用户 session 或任意第三方应用的泛化保证。
- ad-hoc 重签可能改变 TCC 身份；稳定部署应使用固定 Developer ID 和固定安装路径。
