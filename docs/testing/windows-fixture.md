# Windows UIA Rust 原生 fixture

Windows CI 的普通 matrix 会编译 `crates/aad-uia/tests/windows_native.rs`。需要交互桌面的
执行保持显式门禁：手动运行 CI 时选择 `run_windows_native=true`，或可信 push 的提交消息包含
`[windows-native]`。该测试不需要 Python，也不使用 mock UIA。

fixture 与测试 driver 在同一个 Rust 测试进程中运行：fixture thread 用 Win32 创建标题唯一
的顶层窗口和真实 `EDIT`、`BUTTON`、`STATIC` 控件，并运行消息循环；测试 thread 通过
`aad_uia::native_driver()` 访问系统 UIAutomationCore。

覆盖链路包括：

1. `list_windows` 按唯一标题找到 fixture，并核对 HWND；
2. `snapshot`/`find` 读取真实 Control View；
3. 两个同名按钮必须得到 `DRIVER.AMBIGUOUS_MATCH`，且状态保持未变化；
4. `set_value`、`focus`、`invoke` 走真实 UIA pattern，并由 fresh snapshot 回读；
5. `type_text` 通过 Unicode `SendInput` 输入中英文，再由 ValuePattern 回读；
6. `pointer_click` 从 fresh bounds 计算中心点并提交鼠标事件，fixture 状态文本是独立
   postcondition。

本地执行：

```powershell
cargo test --locked -p aad-uia --test windows_native -- --ignored
```

在非 Windows 主机上可交叉编译检查测试与 Win32 API 使用：

```sh
cargo check --locked -p aad-uia --target x86_64-pc-windows-gnu --tests
```

测试必须运行在可交互用户桌面。它不验证 Wine、锁屏、安全桌面、跨完整性级别、RDP 会话
切换、任意第三方控件或所有 IME；这些需要单独资格矩阵。fixture 通过只说明这组受控原生
Win32 控件的 UIA 与输入闭环成立。
