# Windows UIA 原生 fixture 测试

> CI 约束：Windows 原生 job 不随普通 push 或 pull request 自动运行。需要验证时，
> 从 GitHub Actions 手动触发 `CI` workflow，并将 `run_windows_native` 设为 `true`。

`tests/windows/uia_fixture_app.py` 是一个只依赖 Python 标准库 `ctypes` 的小型 Win32
应用。它创建一个标题唯一的顶层窗口，以及原生 `EDIT`、`BUTTON` 和 `STATIC` 控件；其中
一个按钮会更新状态文本，另外两个按钮故意使用相同名称来验证 locator 歧义处理。

`tests/test_windows_uia_native.py` 通过 `ProcessPlugin` 和正式的
`plugins/windows_uia/run.cmd` 入口启动驱动，并使用完整 action ID 执行以下链路：

1. `list_windows` 精确找到 fixture 窗口；
2. `snapshot` 和 `find` 读取真实 UIA Control View；
3. 通过 `ValuePattern`、`SetFocus`、`InvokePattern` 执行 `set_value`、`focus`、`invoke`；
4. 每次写操作后重新获取 snapshot，验证编辑框值、焦点状态和更新后的 `STATIC` 文本；
5. 验证两个同名按钮得到 `DRIVER.AMBIGUOUS`，并通过未变化的状态文本证明没有派发原生点击。
6. 通过 Runtime 执行 `set_value`，再由 `postcondition.observe` 调用真实 `snapshot`，
   验证动作后的新快照包含目标值，同时确认主动作只派发一次。

该测试类在非 Windows 平台整体跳过。手动启用的 GitHub Actions `windows-native` job
安装 `.[windows-uia]` 后执行完整 unittest suite；Linux 和 macOS 不安装 Windows 可选依赖，
也不会尝试模拟原生 UIA。

在 Windows PowerShell 中本地运行：

```powershell
python -m pip install ".[windows-uia]"
python -m unittest tests.test_windows_uia_native -v
```

测试必须运行在可交互的用户桌面会话中。锁屏、安全桌面、跨完整性级别和 RDP 会话切换不属于
此 fixture 的覆盖范围；Wine 也不能替代真实的 `UIAutomationCore` 验证。
