# 使用 Wine 进行 Windows 兼容性测试

> 验证日期：2026-08-24。本文区分“可以在 Wine 上执行的兼容性检查”和“只能在真实 Windows 上完成的平台资格验证”。

## 结论

Wine 可以作为补充测试环境，但不能代替 Windows 真机或 Windows 虚拟机。它适合尽早发现 PE 启动、命令行转义、NDJSON、文件路径、UTF-8、进程退出码以及一部分 Win32 API 兼容问题；不适合证明 Windows UI Automation、UIPI、UAC、Session 0、锁屏、安全桌面、DPI、多显示器、远程桌面或真实应用辅助功能 provider 的行为。

本机实际验证结果：

- Wine 8.0 与 Xvfb 已安装；当前宿主没有图形会话。
- 使用临时 `WINEARCH=win64` prefix 和 `xvfb-run` 可以成功初始化 Wine。初始化时提示缺少 32 位 Wine，因此该环境只能用于 64 位 smoke。
- `wine cmd` 可以启动，Wine 报告兼容的 Windows 版本为 `Microsoft Windows 6.1.7601`。
- 可以启动 Wine 自带 Notepad；用原生 Win32 `EnumWindows` 编译的 64 位探针能够发现标题为 `Untitled - Notepad` 的可见顶层窗口。
- `UIAutomationCore.dll` 文件存在并可由 `LoadLibraryW` 加载，但 `CoCreateInstance(CLSID_CUIAutomation, IID_IUIAutomation)` 返回 `0x80040154`（`REGDB_E_CLASSNOTREG`）。即使尝试通过 `regsvr32` 注册，UIA COM 对象仍不能创建。

因此，当前 Wine 可以验证项目中的 Windows 启动器、协议、Win32 窗口枚举和非 UIA 逻辑；不能运行 `desktop.windows_uia` 的真实 COM 后端，也不能作为 UIA 支持结论的证据。

## 适合放进 Wine CI 的测试

1. 交叉编译并启动最小 PE 控制台程序。
2. 验证 `run.cmd` 的路径引用、参数转发和退出码。
3. 验证 NDJSON 编解码、孤立代理项、超大帧和结构化错误。
4. 验证 Win32 `EnumWindows`、窗口标题、类名和进程 ID 等基础元数据。
5. 验证 Windows 路径、反斜杠、盘符、临时目录和 Unicode 文件名。
6. 验证无需真实 Windows 内核语义的纯逻辑契约，例如 locator、stale、歧义拒绝和权限策略。

这些测试应明确标记为 `wine-smoke`，失败说明存在兼容问题；成功只表示 Wine 中的该项 smoke 通过。

## 必须在真实 Windows 上测试的内容

- `CUIAutomation` COM 激活与 `comtypes` 类型库生成。
- 真实 UIA Control View 遍历、RuntimeId、`CompareElements`。
- `InvokePattern`、`ValuePattern`、`SetFocus` 及动作后的重新观察。
- Windows 10/11、不同架构、32/64 位目标应用组合。
- 普通进程与管理员进程之间的 UIPI 边界。
- UAC、安全桌面、登录、锁屏、Session 0 和多用户会话。
- Job Object 整棵进程树回收。
- DPI 缩放、多显示器、负坐标、远程桌面和显示器热插拔。
- Win32、WPF、WinUI、Electron、Qt、浏览器和 Office 等真实应用矩阵。

## 推荐测试分层

```text
Linux 普通 CI
  ├─ Runtime / Schema / fixture / fake backend 契约
  └─ OCR 固定样例

Linux + Wine + Xvfb
  ├─ Windows 启动器与 PE smoke
  ├─ NDJSON / 路径 / UTF-8 / 退出码
  └─ Win32 窗口枚举

真实 Windows runner 或 VM
  ├─ UIAutomationCore + comtypes
  ├─ fixture app 全动作闭环
  ├─ 权限、UIPI、Job Object、DPI
  └─ 真实应用资格矩阵
```

下一步应优先增加真实 Windows CI runner 和一个标准库 Win32 fixture app；Wine smoke 可同时加入，但应作为较低层的兼容性门，而不是 Windows 驱动的发布门。
