# 只读平台能力探针

> 状态：v1alpha1，基线日期 2026-08-24。Probe 只观察当前进程可见的前置条件，不是 UI 自动化成功证明，也不是 Capability Manifest。

## 目标与调用

运行 `ai-auto-desktop probe` 或 `python -m ai_auto_desktop probe`，stdout 只输出一行 JSON，探测本身完成时退出码为 `0`。能力缺失、权限未授予或环境不完整属于报告内容，不会让命令以错误退出；参数错误或探测器自身未处理的错误仍遵循 CLI 的结构化错误与非零退出约定。

探针不执行以下操作：请求系统权限、弹出 TCC 或 portal 对话框、创建 Wayland RemoteDesktop 会话、打开 `/dev/uinput`、注入键鼠、截图、枚举窗口、读取 accessibility tree 或执行 UI action。Linux 辅助命令只从固定系统目录解析，使用最小化环境启动；不会信任调用者的 `PATH`，也不会把无关环境变量传给子进程。

## JSON 数据合约

顶层 `api_version` 固定为 `ai-auto-desktop.dev/probe/v1alpha1`，`kind` 为 `CapabilityProbe`，`status: completed` 只表示探测流程完成。`platform` 记录规范平台名和版本，`session` 仅依据当前进程可见的环境信号分类，原始 DISPLAY、D-Bus 地址及用户路径不会输出。

`checks` 按稳定名称保存各项结果，每项包含 `state`、面向人的 `summary` 和不含权限内容的 `evidence`。`summary` 只是各状态计数，不汇总成“平台受支持”布尔值。

状态语义：

- `available`：本次只读检查确认了该项狭义前置条件，例如 UIA COM 基础对象可创建、AT-SPI bus 能返回地址或 Wayland socket 存在。它不代表树可读、动作可执行或目标应用兼容。
- `degraded`：发现了一部分前置条件，但当前进程访问不完整或无法完成验证，例如只有 AT-SPI 地址、X11 display 无查询工具、uinput 设备无完整权限。
- `unavailable`：本次上下文中明确未发现该能力，或系统权限明确未授予。它可能随登录 session、安装包、TCC 身份或设备权限变化。
- `unknown`：只读且无提示的方式无法得出结论，例如检查工具缺失、API 不支持安全 preflight、查询超时。不能把 `unknown` 当作可用。

示意输出（字段会随平台不同）：

```json
{
  "api_version": "ai-auto-desktop.dev/probe/v1alpha1",
  "kind": "CapabilityProbe",
  "status": "completed",
  "platform": {"name": "linux"},
  "session": {"kind": "wayland", "interactive": true, "signals": {}},
  "checks": {
    "linux.remote_desktop_portal": {
      "state": "available",
      "summary": "The RemoteDesktop portal interface is exposed; authorization was not requested.",
      "evidence": {"permission_requested": false, "session_created": false}
    }
  },
  "summary": {"available": 1, "degraded": 0, "unavailable": 0, "unknown": 0}
}
```

示例为删减形状；实际输出包含完整平台元数据、全部当前平台 checks 和 `notice`。

## 平台探测边界

### Windows 系统

Windows 报告 6 项检查：`windows.uia`、`windows.session`、`windows.input_desktop`、`windows.integrity`、`windows.dpi`、`script.sandbox`。

- `windows.uia`：加载系统 UIAutomationCore，初始化当前线程 COM，并创建基础 `IUIAutomation` 对象后立即释放。不取得 root element、不枚举窗口或节点、不调用 pattern/action。`available` 仅证明 UIA runtime 的基础 COM 激活在当前进程成功；目标进程提权、UIPI、Session 0、secure desktop、控件语义质量和真实动作均未验证。
- `windows.session`：判断本进程是否位于交互式会话，并区分 Session 0 与远程会话。不附加桌面、不枚举窗口。Session 0 意味着根本没有可交互桌面。
- `windows.input_desktop`：确认本进程位于交互式窗口站，且其线程桌面就是当前输入桌面。不注入任何输入。secure desktop（UAC/登录界面）会使其不可用。
- `windows.integrity`：读取本进程令牌的完整性级别。即使 `available`，UIPI 仍会阻止驱动更高完整性级别的应用——所以它报告的是「本进程能达到的上限」，不是「任意应用都可驱动」。
- `windows.dpi`：报告 DPI 感知状态与由此产生的指针坐标量化步长。缩放显示器上非 DPI 感知会使若干物理像素折叠为一个可寻址点，此时报 `degraded` 并给出量化值，而不是暗示坐标错位。
- `script.sandbox`：见下文「脚本沙箱」。

### macOS 系统

macOS 报告 3 项检查：`macos.accessibility`、`macos.screen_capture`、`script.sandbox`。

- `macos.accessibility` 只调用 `AXIsProcessTrusted()`，不调用带 prompt option 的 API。
- `macos.screen_capture` 只调用 `CGPreflightScreenCaptureAccess()`，不调用 `CGRequestScreenCaptureAccess()`，也不采集画面。
- `script.sandbox`：见下文「脚本沙箱」。macOS 尚无脚本沙箱实现，因此为 `unavailable`。

授权结果绑定当前可执行文件的 TCC 身份。开发路径、签名或 bundle identity 变化后必须重新探测。Accessibility 与 Screen Capture 独立报告；AX 可用不意味着截图可用，反之亦然。旧系统缺少安全 preflight API 时返回 `unknown`。

### Linux 系统

Linux 报告 7 项检查：`linux.at_spi`、`linux.x11`、`linux.wayland`、`linux.remote_desktop_portal`、`linux.libei`、`linux.uinput`、`script.sandbox`。

- `linux.at_spi`：通过 `gdbus` 调用只读 `org.a11y.Bus.GetAddress`；同时记录 AT-SPI 地址、session bus、libatspi 与查询工具是否可见，但不读取 accessibility tree。
- `linux.x11`：记录 DISPLAY、libX11、`xprop`，并通过读取根窗口单个属性做有界元数据查询；不执行 XTEST 或输入注入。
- `linux.wayland`：只检查 WAYLAND_DISPLAY 与对应 socket 元数据，不连接 compositor。
- `linux.remote_desktop_portal`：只读取 `org.freedesktop.portal.RemoteDesktop` 的 `version` 属性，不调用 `CreateSession`、`SelectDevices` 或 `Start`，因此不证明用户会授权或 compositor 会提供 EIS。
- `linux.libei`：检查 libei/liboeffis 和已知诊断命令是否可发现，不建立连接。
- `linux.uinput`：检查设备节点类型与当前进程访问位，并记录 libevdev/辅助命令；绝不打开设备。uinput 可访问也不代表产品允许绕开 compositor 的安全模型。

- `script.sandbox`：见下文「脚本沙箱」。

AT-SPI 语义动作与 X11/Wayland 输入后备是不同能力；一个可用不能补全另一个。Linux 结果还受发行版、桌面环境、compositor、容器设备映射和 session bus 影响，必须分别保留。

### 脚本沙箱

`script.sandbox` 在三端都会报告，因为「本机能否运行 script 步骤、哪些边界真的成立」必须显式回答，不能靠省略暗示。它不执行任何脚本，只检查前置条件。

| state | 含义 |
| --- | --- |
| `available` | 契约承诺的每条隔离边界都被强制。目前只有 Linux（`bwrap` + `prlimit` + `/usr/bin/python3` 齐备）能达到。 |
| `degraded` | 资源上限、进程树回收、空环境、隔离工作目录与隔离解释器均被内核强制，但 `evidence.not_enforced` 列出的边界**没有**被强制。脚本仍会运行。目前 Windows 属于此类，未强制项为 `network` 与 `filesystem`（Windows 无 per-process 网络/mount 命名空间）。 |
| `unavailable` | 本机没有可用沙箱，script 步骤 fail-closed。 |

`evidence.enforced` 与 `evidence.not_enforced` 是这项检查的重点：操作者据此决定是否传 `--allow-scripts`。注意 `state` 与该 flag 相互独立——即使沙箱可用，不传 flag 仍会得到 `SCRIPT.SANDBOX_DENIED`。

evidence **不包含解释器路径**：用户目录下的解释器路径含账号名，会违反本文的「报告不得携带环境标识值」要求，因此只报告 `interpreter_resolved` 这一布尔事实。

`degraded` **不得**被改写为「脚本已被隔离」。详见 `docs/architecture/script-execution.md`。

## 使用规则

Probe 报告适合安装诊断、资格测试环境记录和支持矩阵的前置筛选。正式发布判定还必须在对应真实 OS/session 上运行窗口枚举、树快照、定位、动作、postcondition、权限拒绝与安全桌面负向测试。不得把 probe 的 `available` 改写为“Windows/macOS/Linux 自动化已支持”或真实任务成功率。
