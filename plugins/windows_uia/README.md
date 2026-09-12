# Windows UI Automation 原生能力

`desktop.windows_uia` 已由 [`aad-uia`](../../crates/aad-uia) 以 Rust 实现，并由 CLI/MCP
在 Windows 上直接注册。它通过 `windows` crate 调用 `UIAutomationCore` 和 Win32 API，
不再需要 Python、`comtypes` 或进程 plugin launcher。

能力包括窗口发现、snapshot/describe/overview/find、focus、invoke、set_value、显式
`type_text`、显式 `pointer_click`，以及交互录制的 watch/collect/release。每个写动作都只接受
当前 snapshot 的 target，派发前重新定位并核对 live UIA identity；多义、过期、protected、
无效 bounds 或前台窗口不一致时失败关闭。

`set_value` 与 `invoke` 优先使用 UIA Value/Invoke/Toggle/LegacyIAccessible pattern。
`type_text` 和 `pointer_click` 必须由调用方显式选择，使用 `SendInput`，不会成为语义动作的
隐式 fallback。任何部分派发或派发后无法确认的情况都按不可重试的
`DRIVER.UNKNOWN_EFFECT` 处理；成功也必须由 fresh snapshot/postcondition 验证。UIPI、
secure desktop、锁屏、跨 session 与更高完整性级别窗口不在支持范围内。

Windows CI 会运行 [`windows_native.rs`](../../crates/aad-uia/tests/windows_native.rs)：测试用
Rust 创建真实 Win32 `EDIT/BUTTON/STATIC` 控件，再通过正式 Rust driver 验证窗口枚举、树快照、
歧义拒绝、ValuePattern、InvokePattern、Unicode SendInput 和中心点鼠标点击。

```powershell
cargo test --locked -p aad-uia --test windows_native
```

完整契约与安全边界见
[`windows-uia-driver.md`](../../docs/architecture/windows-uia-driver.md)。
