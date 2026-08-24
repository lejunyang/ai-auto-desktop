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
安装 `.[windows-uia]` 后，通过 `tests/windows/run-native-fixture.ps1` 运行
`python -m unittest tests.test_windows_uia_native -v`；Linux 和 macOS 不安装 Windows 可选依赖，
也不会尝试模拟原生 UIA。PowerShell runner 会保留 unittest 的退出码，因此 fixture 失败时
job 仍然失败，不会因留存报告而被误判为通过。

## 下载并核验 CI 结果

只有从 GitHub Actions 页面手动运行 `CI`，并显式选择
`run_windows_native=true`，才会创建 `windows-native` job。普通 push、pull request 和未勾选该
输入的手动运行都不会启动 Windows runner。选择要验证的 branch 或 commit 后，可按以下步骤取证：

1. 打开该次 workflow run，在页面底部的 **Artifacts** 区域下载
   `windows-native-fixture-result-<run_id>-<run_attempt>`。
2. 解压后读取 `windows-native-fixture-result.json`。即使 unittest 失败，上传步骤也会通过
   `if: always()` 尝试执行；若报告文件本身未生成，上传步骤会明确报错，而不是静默缺失。
3. 将报告中的 `commit_sha` 与 workflow run 页面显示的 commit 完整 SHA 对照，并确认
   `runner`、`os` 和 `python` 符合预期的 Windows runner 环境。
4. 检查 `test.command` 是否为固定的原生 fixture 命令，并联合核验 `test.result`、
   `test.exit_code` 和顶层 `status`。`passed` 必须对应退出码 `0`；`failed` 对应 unittest
   非零退出码；`error` 表示测试命令未能正常完成。`timestamp` 是报告写入时的 UTC 时间。

也可以使用 GitHub CLI 下载（artifact 名中的 attempt 可在 run 页面确认）：

```sh
gh run download <run-id> \
  --name windows-native-fixture-result-<run-id>-<run-attempt> \
  --dir windows-native-result
python -m json.tool windows-native-result/windows-native-fixture-result.json
```

在 PowerShell 中做最小一致性检查：

```powershell
$result = Get-Content windows-native-fixture-result.json -Raw | ConvertFrom-Json
$result.commit_sha -eq "<workflow 页面上的完整 commit SHA>"
$result.test.command
$result.test.result
$result.test.exit_code
$result.status
```

JSON 使用固定字段白名单，只包含 commit SHA、runner/OS/Python 元数据、测试命令和结果、
UTC 时间及状态；不会收集完整环境变量、GitHub event payload、测试 stdout/stderr 或凭据。
测试详细输出仍在该次 Actions job 的日志中。该报告是一次 fixture 执行记录，不是签名证明，
也不能扩大为对任意 Windows 应用、提权窗口或 secure desktop 的资格声明。

在 Windows PowerShell 中本地运行：

```powershell
python -m pip install ".[windows-uia]"
python -m unittest tests.test_windows_uia_native -v
# 或生成与 CI 同结构的本地报告（退出码仍等于测试退出码）
pwsh -File tests/windows/run-native-fixture.ps1
```

测试必须运行在可交互的用户桌面会话中。锁屏、安全桌面、跨完整性级别和 RDP 会话切换不属于
此 fixture 的覆盖范围；Wine 也不能替代真实的 `UIAutomationCore` 验证。
