# Windows 用户界面自动化（UIA）进程驱动

`desktop.windows_uia` 是仅支持 Windows 的 NDJSON 进程能力。它使用可选的 `comtypes` 包和
生成的 `UIAutomationClient` 类型库，执行原生 `SetFocus`、`InvokePattern` 和 `ValuePattern`
操作。工作流必须在 `requires.permissions` 中声明 `desktop.observe`；对于写操作，还必须声明
`desktop.input`。宿主也必须显式授予这些权限。

在 Windows 上使用 `run.cmd` 启动，或使用显式的 Python 命令参数：

```text
python plugins\windows_uia\windows_uia_driver.py
```

`run.sh` 仅用于跨平台协议测试和模拟后端测试。在非 Windows 宿主上，真实操作会以
`DRIVER.UNAVAILABLE` 失败，但仍可协商清单。该驱动不会注入键盘或指针输入、截取屏幕截图，
也不会运行 OCR。
