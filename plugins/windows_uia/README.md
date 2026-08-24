# Windows 用户界面自动化（UIA）进程驱动

`desktop.windows_uia` 是仅支持 Windows 的 NDJSON 进程能力。它使用可选的 `comtypes` 包和
生成的 `UIAutomationClient` 类型库，执行原生 `SetFocus`、`InvokePattern` 和 `ValuePattern`
操作，并提供显式的 `desktop.windows_uia.type_text@1` 键盘输入后备。工作流必须在
`requires.permissions` 中声明 `desktop.observe`；对于写操作，还必须声明
`desktop.input`。宿主也必须显式授予这些权限。

`type_text` 必须显式调用，绝不会从 `set_value` 自动退化。它要求调用方提供同一当前快照的
`target`、原始 `locator` 和 `text`；驱动会重新抓取并唯一解析 UIA 树、核对原生身份、拒绝
密码或 protected 元素、设置焦点并确认目标已获得键盘焦点，且前台窗口的 HWND 与 fresh
snapshot 的顶层窗口 handle 完全一致、PID 与目标一致，然后使用 Windows `SendInput` 的
Unicode 键盘事件输入文本。每个 Unicode scalar 独立成批（非 BMP 代理对同批）发送，每批前
都会重新检查焦点、前台 HWND 和 PID。
文本不能为空，最多 1024 个 Unicode 字符，必须是合法 UTF-16，且不能包含换行、Tab、ESC
等 Unicode 控制字符、格式字符、私用区或其他非普通文本码位。该动作只输入字面普通文本，
不支持快捷键、组合键或按键脚本，也不提供
任何 pointer/mouse 后备。成功仅表示 Windows 接受了输入事件，调用方仍应使用新快照验证结果。

`SendInput` 受 UIPI、进程完整性级别、桌面/session 与前台焦点约束：普通权限进程通常不能
向提权窗口、UAC secure desktop、登录或锁屏界面注入；窗口切换或用户抢占焦点也可能让输入
落到错误位置。`KEYEVENTF_UNICODE` 不依赖当前键盘布局来映射字符，但目标应用、输入法和控件
仍可能改变对事件的解释。首个 `INPUT` 尚未提交前的失败标记为文本 `not_applied`（此前的
`SetFocus` 仍可能已改变焦点）；一旦至少一个事件已提交，后续上下文漂移、部分提交或超时
返回不可重试的 `DRIVER.UNKNOWN_EFFECT`，调用方不得自动重放。分批复检缩小了竞争窗口，但
`SendInput` 与焦点检查不是原子操作，仍无法彻底消除用户在两者之间抢走焦点的竞态。

在 Windows 上使用 `run.cmd` 启动，或使用显式的 Python 命令参数：

```text
python plugins\windows_uia\windows_uia_driver.py
```

`run.sh` 仅用于跨平台协议测试和模拟后端测试。在非 Windows 宿主上，真实操作会以
`DRIVER.UNAVAILABLE` 失败，但仍可协商清单。除显式 `type_text` 外，该驱动不会注入键盘；
它永远不会注入指针输入、截取屏幕截图或运行 OCR。
