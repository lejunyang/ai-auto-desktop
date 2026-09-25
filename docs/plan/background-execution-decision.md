# 后台执行方案决策：不抢输入、不占前台桌面

> 文档类型：可决策方案论证（decision doc）。产出于 2026-09-25。
>
> **目标**：为 `ai-auto-desktop`（Rust，Windows 桌面 UI 自动化工具，主路径为 UIA 语义动作 L0）确定一条"能在后台运行、不抢用户前台焦点、不改变输入桌面"的可落地路线，并给出三端方案骨架与待真机实测清单。
>
> **前置事实（不在本文档重复论证，直接引用）**：本文所有 Windows 结论建立在已闭环的真机实测之上，实测记录见 `docs/research/input-synthesis-and-background-execution.md` §6。核心前提有四条：
> 1. `SendInput` 跟随**输入桌面（input desktop）**，与调用线程桌面无关；在 `CreateDesktop` 建的隐藏桌面上，即便 `SetThreadDesktop` 成功、即便进程出生就在该桌面，`SendInput` 一律返回 0 / `ACCESS_DENIED`；只有 `SwitchDesktop` 让它成为输入桌面（=用户当场黑屏）后才成功 → **"隐藏桌面 + 合成输入"的 Windows 后台方案已被实测否决**。
> 2. 隐藏桌面**观察侧**实测正常：`EnumDesktopWindows` 能枚举、`PrintWindow(PW_RENDERFULLCONTENT)` 能截到与可见桌面几乎一致的内容 → 引出替代路"隐藏桌面 + 纯 L0(UIA) 语义动作"。
> 3. `KEYEVENTF_UNICODE` 对 Raw Input / `GetAsyncKeyState` 不可见；修饰键泄漏确认不自愈（键鼠补全范畴，主 agent 在做，本文仅作背景）。
> 4. 实测环境：Windows 11 家庭版 26200、未提权（Medium 完整性）、本地交互式 Session 1。macOS / Linux 均无真机可用。
>
> **本文档的调研环境**（只读探查，未做任何抢输入/改桌面写操作，2026-09-25 复核）：`OpenInputDesktop` 报告 `Default`，`GetThreadDesktop` 报告 `Default`，`ProcessIdToSessionId` 报告 Session 1，完整性 Medium，OS 为 Windows 11 家庭版 10.0.26200 x64——即"未提权个人开发机"这一目标场景本身，结论据此落地。

---

## 0. 一句话结论

**在"未提权个人开发机（Win11 家庭版、本地交互 Session 1）"这一实际场景下，Windows 端不存在"既完全后台隔离、又能用合成键鼠"的可落地方案。** 推荐路线是：**主用"隐藏桌面(CreateDesktop) + 纯 L0/UIA 语义动作"承担 agent 自开自用型后台任务；对 L0 覆盖不到的动作，退化为"前台执行 + 明确占用提示"，而不是去上第二会话或驱动级方案。** 「第二 Windows 会话」是能力最完整、但对个人机而言过重且被客户端 SKU 卡死的路径，只保留为企业/服务器形态的备选，不进个人机主线。

判据速览：

| 候选路 | 能做合成键鼠 | 完全隔离(用户看不见) | 未提权个人机可用 | 客户端 SKU 限制 | 与 L0 主路径契合 | 改造成本 | 个人机推荐 |
|---|---|---|---|---|---|---|---|
| (a) 隐藏桌面 + 纯 L0/UIA | ❌ 否（实测否决合成输入） | ⚠️ 部分（观察隔离，但仍在同会话） | ✅ 是 | 无（家庭版即可） | ✅ 极高（L0 本就是主路径） | 中 | **主选** |
| (b) 第二 Windows 会话 (PiP/Child Session) | ✅ 是（独立输入桌面） | ✅ 是 | ❌ 否（需凭据 + 并发限制） | ⚠️ 大（客户端版不支持多用户并发会话） | ⚠️ 中（需整套会话管理） | 高 | 备选/企业形态 |
| 前台执行 + 占用提示 | ✅ 是 | ❌ 否 | ✅ 是 | 无 | ✅ 高 | 零 | **兜底默认** |

---

# Part 1 · Windows 两条后台候选路

## 1.1 候选路 (a)：隐藏桌面(CreateDesktop) + 纯 L0/UIA 语义动作

### 机制回顾

`CreateDesktopW("AadAutomation")` 在同一 `WinSta0` 下建立一个新的 Desktop 对象；把目标进程 `CreateProcessW(..., si.lpDesktop = "AadAutomation")` 起在该桌面上，它不出现在用户可见的 `Default` 桌面里。Desktop 是一个 UI 消息处理域，决定哪些窗口能收鼠标/键盘输入。隔离本身是真的——只是**输入这一环被输入桌面机制卡死**。

### 能做 / 不能做（区分观察侧与动作侧）

**能做（已实测，见前置事实 §6.2/§6.3）：**

- **进程隔离**：`CreateDesktopW` / `CreateProcessW(lpDesktop=...)` 均成功，目标进程真实起在隐藏桌面，不干扰用户的 `Default` 桌面（实测 `foreground_unchanged=True`、`cursor_unchanged=True`）。
- **窗口枚举**：`EnumDesktopWindows(hDesktop, ...)` 正常枚举该桌面窗口。
- **截图**：`PrintWindow(hwnd, PW_RENDERFULLCONTENT)` 在隐藏桌面上取到的内容与可见桌面几乎一致（192 点采样，控件边框色两侧都在）。GDI 绘制的普通窗口渲染正常。

**能做（原理成立，但本项目尚未在隐藏桌面上实测——标 [待验证]）：**

- **L0/UIA 语义动作**：`IUIAutomationElement::SetFocus`、`InvokePattern`、`TogglePattern`、`ValuePattern::SetValue`、`LegacyIAccessible` 默认动作，以及 `ScrollPattern`/`SelectionItemPattern`。这些走 COM/UIA 消息通道，**不依赖输入桌面的输入队列**，原理上应当在隐藏桌面可用。本项目 `crates/aad-uia/src/windows.rs` 里 `focus`/`invoke`/`set_value` 就是这一层，主路径与它天然对齐。**[待验证]** — winforms-mcp 声称"UIA 全部操作正常"，但这是本方案的地基，必须在本项目自己的 driver + 隐藏桌面组合下亲测覆盖率（见 §4 实测项 W-1）。

**不能做（已实测否决）：**

- **任何合成键鼠**（`SendInput` 的键盘/鼠标、`type_text` 的 `KEYEVENTF_UNICODE`、`pointer_click` 的 `MOUSEEVENTF_ABSOLUTE`）在隐藏桌面上一律 `ACCESS_DENIED`。这意味着 **L0 表达不了的动作，在本方案的后台模式下就是做不到**：
  - 无 UIA Pattern 的自绘控件（画布、部分游戏 UI、Canvas 类）；
  - 需要修饰键组合的快捷键（Ctrl+C / F5 / Alt+Tab 等）；
  - 需要"元素内相对位置"的滑块/色板/时间轴拖拽；
  - 悬停触发的 tooltip/hover 菜单。

**不能做（硬边界，与是否隐藏桌面无关）：**

- 目标进程完整性级别高于自身时，UIPI 让 UIA 枚举拿到空树、注入被拒（未提权个人机上 = 无法操作以管理员运行的程序）。
- 锁屏 / UAC / 登录界面运行在独立 `Winlogon` 桌面，任何用户态路径都到不了，应 fail fast。

### 能力边界

- **只能"agent 自开自用"**：只能操作在这个隐藏桌面上由 agent 亲自启动的进程，**无法接管用户 `Default` 桌面上已有的窗口**（Desktop 之间窗口不共享）。这是所有隔离方案的共性边界，不是本方案独有缺陷。
- **GPU 加速应用渲染未知**：§6.3 的渲染结论来自 GDI 简单窗口；Chrome / Electron 这类 DWM 合成 + GPU 加速应用在隐藏桌面（无 DWM 合成上屏）上的 `PrintWindow` 表现**未验证**，可能出现黑屏/空帧（见 §4 实测项 W-2）。这直接决定"隐藏桌面能不能自动化浏览器/Electron 应用"。
- **动作后验证仍靠重新观察**：L0 可回读 Pattern 值，postcondition 机制在此价值翻倍。

### 权限与客户端 SKU 限制

- **权限**：`CreateDesktop` / `CreateProcess(lpDesktop)` / UIA 均**不需要提权**，Medium 完整性即可。已在本机（家庭版、未提权）验证隔离侧可用。
- **SKU**：**家庭版/专业版/企业版无差异**——Desktop 对象是 Win32 基础设施，不区分客户端 SKU。这正是它对个人机友好的关键点。**唯一的 SKU 相关限制来自 UIPI（跨完整性级别），与 SKU 无关而与提权状态有关。**

### 与本项目 L0(UIA) 主路径的契合度

**极高。** 本项目安全主轴是"任何写操作必须锚定一个已观察且二次校验过的元素，MCP schema 禁止裸坐标"，L0 正是这条主轴的原生实现（`snapshot_id + revision + node_id` 重定位 + role/name/automation_id 比对）。隐藏桌面 + 纯 L0 等于"把已有主路径搬到一个隔离桌面里跑"，安全模型零改动。合成输入本就是补充，不是主路径，后台模式下失去它属于"能力子集"而非"能力倒退"。

### 改造成本（中）

- **必需**：driver/session manager 增加"目标桌面"维度——在指定 Desktop 上 `CreateProcess`、`SetThreadDesktop` 到该桌面后建立 UIA、`EnumDesktopWindows` 限定枚举范围、`PrintWindow` 走该桌面窗口。
- **必需**：capability-probe 增加"隐藏桌面 L0 可用性"这一 check（现有 probe 只报 `windows.input_desktop` 是否为输入桌面，需扩展一项"非输入桌面上 UIA 是否可建/可动作"）。
- **必需**：后台模式下对合成输入路径 fail-closed，返回结构化错误（如 `DRIVER.BACKGROUND_INPUT_UNSUPPORTED`）并提示"该动作需前台模式"，而不是静默失败或误报成功。
- **不需要**：新依赖、新权限、新驱动、签名。

### 未提权个人机场景推荐

**主选。** 这是唯一在"家庭版 + 未提权 + Session 1"下零权限成本、且与现有安全模型完全契合的后台路径。前提是先用一个 spike 把两件事测清楚（W-1 UIA 覆盖率、W-2 Electron/Chrome 渲染），再决定产品化边界。

---

## 1.2 候选路 (b)：第二 Windows 会话（UiPath PiP / Picture-in-Picture Child Session）

### 机制

UiPath 的成熟做法：以画中画（PiP）模式启动流程时，**新开一个独立的 Windows 登录会话（Child Session）**，机器人在这个隔离会话里跑，用户继续用自己的会话与桌面。关键区别要说清楚，UiPath 同时提供两个不同实现：

- **Child Session（独立会话）**：新建一个真正的 Windows Session，有**自己独立的输入桌面**，因此 `SendInput` 等合成输入在该会话内可用，不受 §1.1 的输入桌面限制。这是本候选路的本体。
- **Virtual Desktop（同会话虚拟桌面）**：本质就是 §1.1 的 Desktop 对象方案，**合成输入在其上不可用**（与本项目实测一致）。UiPath 之所以同时提供 Child Session 版本，很可能正是因为 Virtual Desktop 版本的输入受限。

### 能做 / 不能做

**能做：**

- **完全隔离**：用户看不见也不受影响，agent 在独立会话里有完整的自己的桌面 + 输入桌面。
- **合成键鼠可用**：独立会话有自己的输入桌面，`SendInput` / `pointer_click` / 组合键在该会话内正常——这是它相对候选路 (a) 的唯一实质优势。**[待验证]** — 本项目未实测第二会话（个人机 SKU 限制导致难以搭建，见下）。
- **L0/UIA 亦可用**：独立会话内 UIA 照常工作。

**不能做：**

- **无法接管用户现有窗口**：独立会话是另一套桌面，用户 `Default` 会话里的窗口不可见——与所有隔离方案同样的边界。
- **需要凭据认证**：启动新会话需要 Windows 账号凭据（UiPath 明确要求认证新会话）。无人值守场景需要预置凭据，带来 secret 管理负担。

### 权限与客户端 SKU 限制（这是它在个人机上的致命伤）

- **客户端版不支持真正的多用户并发会话**：Windows 10/11 的 **家庭版 / 专业版** 在设计上限制为单一交互式会话——当第二个会话被激活，第一个会话会被断开/锁定（这正是本机会话切换的默认行为）。要在"用户继续用会话 1、agent 同时在会话 2 跑"意义上真正并发，**需要 Windows Server（多会话/RDS）或经过第三方修改的多会话组件**。
  - **家庭版**：受限最严，无 RDS、无组策略层的会话管理，本候选路基本不成立。
  - **专业版**：可作单会话 RDP 主机，但仍不支持"本地控制台 + 远程会话并发交互"，且并发多会话需破解或非官方手段。
  - **企业版 / Server**：RDS/多会话是官方能力，本候选路才真正可行。
- **提权**：创建/管理会话通常需要更高权限或专门服务，抬高整体权限面。

### 与本项目 L0 主路径的契合度（中）

会话内 UIA + 合成输入都可用，功能上契合。但它引入了一整套本项目当前没有的东西：**会话生命周期管理、凭据存储、跨会话的 single-writer 协调**。这些与"装在开发者个人机上的轻量工具"定位不符。

### 改造成本（高）

需要：会话创建/销毁编排、凭据安全存储、会话内 driver 启动与健康监控、跨会话 lease。对个人机场景，投入产出比很低。

### 未提权个人机场景推荐

**不推荐进个人机主线。** 在"家庭版 + 未提权"下，本候选路因 SKU 并发限制而**基本不成立**；即使在专业版上，官方手段也无法达成"控制台会话与 agent 会话真并发"。它是**唯一能同时满足"完全隔离 + 合成键鼠可用"的 Windows 路径**，因此保留为**企业版/Server 部署形态的备选**，但不作为个人开发机的目标。

---

## 1.3 Windows 其他路线（明确不做，简述理由）

| 路线 | 结论 | 理由 |
|---|---|---|
| `SwitchDesktop` 式"后台" | ❌ 不做 | 让用户当场黑屏，与"不抢前台"直接矛盾；只保留调试用途。 |
| Session 0 服务 | ❌ 不做 | 服务跑在 Session 0 无法驱动用户桌面，`SendInput`/UIA `FromHandle` 对跨会话 HWND 全失效。 |
| 驱动级 (Interception / ViGEmBus) | ❌ 不做 | 需内核驱动 + SYSTEM，与"以观察为前提、拒绝盲点击"的定位自相矛盾；反作弊冲突；部署成本极高。 |
| PostMessage 通用后备 | ❌ 不做 | WinUI3/UWP 无子 HWND、现代框架需完整消息序列，失败方式是"静默无反应"，最坏的失败方式。 |
| IddCx 虚拟显示器 | ⚠️ 不解决本问题 | 它解决"没有物理显示器"，不解决"抢输入"；后台隔离需要的是独立输入队列/会话，不是多一块屏。 |

---

## 1.4 Windows 最终建议（未提权个人机）

```
后台任务分派决策：
  ├─ 目标动作全部可用 L0/UIA 表达（Invoke/SetValue/Toggle/Scroll/Select）？
  │    └─ 是 → 【隐藏桌面 + 纯 L0】后台执行，不抢输入，不占前台          ← 主选 (a)
  │    └─ 否（需合成键鼠：组合键 / 拖拽 / 自绘控件 / 悬停）
  │         └─ 【前台执行 + "执行期间请勿操作"提示】                      ← 兜底默认
  └─ 需要"完全隐藏 + 合成键鼠"且部署在企业版/Server？
       └─ 【第二 Windows 会话 (Child Session)】                          ← 备选 (b)，非个人机主线
```

**诚实度要求**：capability-probe 必须能分别报告"当前会话是否为输入桌面""隐藏桌面上 L0 是否可建/可动作""目标完整性级别是否可达"，不能用一个 `windows: ok` 概括。probe 的 `available` 不得被改写为"Windows 后台自动化已支持"。

---

# Part 2 · macOS 与 Linux 方案骨架

> 三端后台机制完全不同，不存在统一抽象。以下为骨架 + 待验证清单，**均无真机，全部标 [待验证]**，并注明所需环境。

## 2.1 macOS：私有 SkyLight 路径（骨架 + 待验证清单）

### 方案骨架

macOS 上"同一 login session 内保持用户焦点不变、向后台应用发事件"**没有任何公开 API 能做到**（已有开源项目尝试后回退整个分支）。现实可行的唯一路径依赖私有 SkyLight 框架三个未文档化符号：

1. **`SLEventPostToPid`**：走 auth-signed 的 SkyLight 通道（绕开 `IOHIDPostEvent`），Chrome/Electron 的 renderer IPC 过滤器**接受**从这条通道来的事件（公开的 `CGEvent.postToPid` 会被静默丢弃）。
2. **`SLPSPostEventRecordTo`**：借 yabai 的 focus-without-raise 手法，翻转目标 App 的 AppKit active 状态而**不**把窗口提到前面、不触发切 Space。
3. **`_AXObserverAddNotificationAndCheckRemote`**：Electron 窗口被遮挡时 Blink 暂停 AX 树更新，此私有变体把观察者标记为 remote-aware 以维持更新。

这套组合已由开源 `cua-driver` 实证可用。**代价**：三符号全部未文档化，靠 `dlopen`/`dlsym` 获取；系统更新可能失效；无法上架 App Store；无官方支持。

**建议形态**：做成显式可选的"第二档后台能力"，默认不启用；probe 明确报告当前 macOS 版本上三符号是否可解析；失效时优雅降级到"前台激活再操作"。

**已知硬边界（即便上了 SkyLight 也不可解）**：

- Chromium 把合成右键在网页内容上强制降级成左键（AX 可寻址目标走 AX action 无此问题）。
- Canvas 类应用（Blender GHOST、Unity、游戏）完全过滤 per-pid 路由，必须短暂前台激活——"不抢焦点"承诺在这类应用上破功。

**权限模型**：TCC 三权分立（PostEvent / ListenEvent / ScreenCapture）分别申请；签名身份必须稳定，否则每次重签需重新授权。`EnableSecureEventInput`（密码输入场景）会吞掉合成输入，需能检测并明确报错。

**替代（真隔离）骨架**：第二 login session（`launchctl asuser` + 第二用户账号）或 `vz` 框架跑 macOS VM。前者受快速用户切换限制、后者最重最干净。

### 待验证清单（需 macOS 真机）

| # | 待验证项 | 判据 | 所需环境 |
|---|---|---|---|
| M-1 | 三个 SkyLight 符号在当前 macOS 版本上是否可 `dlsym` 解析、可用 | 三符号非空指针，且能对后台 App 投递被接收 | macOS 真机（多版本，尤其 macOS 26） |
| M-2 | macOS 26 上 `CGEvent.postToPid` 是否真的收紧（Safari WebKit 内容收不到） | 授予 Accessibility 后仍收不到 → 证实收紧 | macOS 26 真机 |
| M-3 | SkyLight 路径下光标是否真的不动、焦点/Space 是否不变 | 投递期间用户前台 App 与鼠标位置不变 | macOS 真机 |
| M-4 | Canvas 类应用（Unity/Blender）破功的实际范围 | 记录哪些应用必须前台激活 | macOS 真机 + 目标应用 |
| M-5 | TCC 三权在稳定签名身份下的授权持久性 | 重启/重签后是否需重新授权 | macOS 真机 + 开发者签名证书 |

**建议**：macOS 上先不承诺后台能力，先把前台模式的输入补全做扎实；后台作为独立课题，等 SkyLight 路径在多版本上验证稳定后再评估。

## 2.2 Linux：Xvfb / headless compositor（骨架 + 待验证清单）

### 方案骨架

Linux 上"另开一个 display"是常规操作，**同时解决"完全隔离"与"无物理显示器"**，是三端里最顺的。核心是：**只能操作在这个 display 上由 agent 启动的进程，不能接管用户现有窗口**。

| 方案 | 形态 | 适用 |
|---|---|---|
| **Xvfb** | 虚拟 X server，无显示输出 | X11 应用，最成熟；本项目已有"私有 Xvfb + 私有 AT-SPI bus"验证 `type_text` 的实践（KCalc 隔离闭环） |
| **sway headless** | wlroots compositor，`WLR_BACKENDS=headless`，`swaymsg create_output` 建 HEADLESS-N | Wayland 多窗口 + wlr 虚拟输入协议可用 → Wayland 上最顺组合 |
| **mutter --headless --virtual-monitor** | GNOME 无头 | 需 GNOME 语义；输入走 RemoteDesktop portal |
| **cage** | 单应用 kiosk | 只跑一个程序时最简单 |

输入通道随 display 走：X11 → XTEST（现有实现，最成熟）；wlroots → `zwp_virtual_keyboard_v1` + `zwlr_virtual_pointer_v1`（无提示）；GNOME/KDE Wayland → RemoteDesktop portal + libei（**每次弹窗、无持久授权，无人值守基本不可用**）；兜底 → uinput（需 root/input 组，发键码非符号，有布局问题）。

**已有工程资产**：仓库已有 Xvfb + 私有 AT-SPI bus 实践（KCalc 22.12.3 隔离语义计算闭环），扩展为后台产品能力的增量不大。

### 待验证清单（需 Linux 真机）

| # | 待验证项 | 判据 | 所需环境 |
|---|---|---|---|
| L-1 | Xvfb + AT-SPI 下 agent 自开应用的语义动作 + XTEST 输入完整闭环 | 起应用→snapshot→动作→fresh snapshot 回读一致 | Linux X11 真机（KDE Plasma 5.27 基线） |
| L-2 | sway headless + `zwlr_virtual_pointer` / `zwp_virtual_keyboard` 是否无提示可用 | compositor 广告该协议且注入无弹窗 | Linux + sway/wlroots |
| L-3 | GNOME/KDE Wayland 的 RemoteDesktop portal 是否真无持久授权 | 每次 session 是否强制弹窗 | Linux + GNOME 48 / KDE Plasma |
| L-4 | KDE Plasma 6.5 `pointer-warp-v1` 指针支持是否已落地 | 协议是否被广告、指针能否 warp | Linux + KDE Plasma 6.5 |
| L-5 | headless display 下 GPU 加速应用（Chromium）渲染/截图是否正常 | 截图非黑屏、内容正确 | Linux 真机 + 目标应用 |

---

# Part 3 · 待主 agent 真机串行执行的实测项清单

> **执行纪律（强约束）**：以下每一项都可能需要真机 GUI 独占（切输入桌面、抢焦点、独占渲染），因此**必须由主 agent 串行执行，不能与其他 GUI 实测并行**。脚本一律用 pwsh7 驱动；探针本体用 Python（本项目 `tests/windows/` 既有风格，ctypes 直调 Win32）。每项给"脚本设计思路 + 预期判据"，可直接落地。
>
> **前置提醒**：本清单是本文 §1.1 / §2 中标 [待验证] 项的可执行化。W-1 / W-2 是候选路 (a) 能否产品化的地基，优先级最高。

## W-1 · 隐藏桌面上 UIA(L0) 动作的实际覆盖率 [最高优先级]

- **目的**：验证 §1.1 的地基——纯 L0 语义动作在**非输入桌面**上是否真能建立 UIA 并执行动作。
- **脚本设计思路**（pwsh7 编排 + Python 探针）：
  1. pwsh7 调用 Python：`CreateDesktopW("AadUiaTest")`，`CreateProcessW(lpDesktop="AadUiaTest")` 起一个带标准控件的 fixture（优先复用 `crates/aad-uia/tests` 的 Win32 fixture；否则起 `notepad.exe` / `calc.exe`）。
  2. Python 内 `SetThreadDesktop` 到该桌面后 `CoInitialize` + 建 `CUIAutomation`，用 `ElementFromHandle`（对 `EnumDesktopWindows` 枚举到的 hwnd）取根元素。
  3. 依次尝试：树枚举、`SetFocus`、`InvokePattern.Invoke`、`ValuePattern.SetValue` + 回读、`TogglePattern.Toggle`、`ScrollPattern`、`SelectionItemPattern.Select`。
  4. 每个动作后回读 Pattern 值 / 重新 snapshot 验证是否生效，逐动作记账 `{pattern, dispatched, postcondition_verified}`。
  5. 全程用 `OpenInputDesktop` 确认输入桌面仍为 `Default`（未黑屏用户），并断言 `SendInput` 在该桌面仍 `ACCESS_DENIED`（作为对照）。
- **预期判据**：
  - **通过** = 树枚举成功 + `Invoke`/`SetValue`/`Toggle` 至少三类动作 dispatched 且 postcondition 回读一致 + 输入桌面全程 `Default`。
  - **部分** = 部分 Pattern 可用，产出"隐藏桌面 L0 覆盖率表"，据此界定后台模式能力子集。
  - **否决** = 隐藏桌面上无法建立 UIA 或所有动作不生效 → 候选路 (a) 不成立，回退前台模式。

## W-2 · Chrome / Electron 在隐藏桌面的渲染与截图 [最高优先级]

- **目的**：§6.3 渲染结论仅覆盖 GDI 简单窗口；GPU 加速 + DWM 合成应用未知，直接决定"隐藏桌面能否自动化浏览器/Electron"。
- **脚本设计思路**：
  1. pwsh7 + Python：在隐藏桌面 `CreateProcessW(lpDesktop=...)` 起 Chrome（`--new-window about:blank` 或本地测试页），等待窗口出现（`EnumDesktopWindows` 轮询到目标 hwnd，带超时）。
  2. `PrintWindow(hwnd, PW_RENDERFULLCONTENT)` 截图，密集采样（≥192 点，参考 `verify_render_and_modifier.py` 已修正的采样密度，不要只取 4 点）统计 distinct 颜色数。
  3. 对照组：同一 URL 在可见桌面截一份，比较颜色分布相似度。
  4. 记录是否黑屏/空帧、内容是否与可见桌面一致。
- **预期判据**：
  - **通过** = 隐藏桌面截图 distinct 颜色数与可见桌面同量级，主体内容可辨（非纯黑/纯白）。
  - **失败** = 黑屏或空帧 → 隐藏桌面不适用于浏览器/Electron，需在文档中明确排除该类应用，或改走 Chrome headless / CDP 而非窗口截图。
- **注意**：本项须 GUI 独占（起真实浏览器），务必串行，不与主 agent 键鼠实测并行。

## W-3 · 提权后能否操作提权窗口（UIPI 边界） [高优先级]

- **目的**：§6 全部结论在"未提权"下取得；验证提权后 UIPI 边界的实际行为，为"目标以管理员运行"给出准确报错文案。
- **脚本设计思路**：
  1. 未提权侧：Python 用 UIA 尝试枚举一个以管理员运行的窗口（如提权的记事本），断言拿到空树 / `ACCESS_DENIED`。
  2. 提权侧（pwsh7 以管理员启动 Python）：对同一提权窗口重试 UIA 枚举 + `Invoke`，记录是否成功。
  3. 记录 `GetTokenInformation` 完整性级别对照。
- **预期判据**：未提权拿空树 / 提权成功 → 证实"同完整性级别内可操作"，据此让 driver 在检测到目标完整性更高时 fail fast 并给出"请以同等权限启动"建议。**需一次提权（UAC 确认）。**

## W-4 · 低级输入钩子存在时新键盘路径的表现 [中优先级]

- **目的**：本项目已知 `DRIVER.INPUT_BLOCKED`（存在低级钩子时 `SendInput` 整批拒绝）；验证补了虚拟键/扫描码路径后表现是否一致。
- **脚本设计思路**：
  1. 装/起一个已知会装 `WH_KEYBOARD_LL` 的软件（或用 Python 自装一个 LL hook 制造条件）。
  2. 分别用 `KEYEVENTF_UNICODE`、虚拟键、扫描码三种方式 `SendInput`，记录返回值 / `GetLastError` / 探针三通道可见性。
- **预期判据**：记录三种编码在有钩子环境下是否整批拒绝、是否文字错乱 → 决定是否需要"整批失败即报 `INPUT_BLOCKED`"策略对新路径同样适用。**本项仅涉合成输入，属主 agent 键鼠实测范畴，可并入其串行队列。**

## W-5 · 隐藏桌面 session manager 端到端 smoke [产品化前]

- **目的**：把 W-1/W-2 结论串成"agent 自开自用后台任务"的最小闭环，验证 session 生命周期。
- **脚本设计思路**：
  1. pwsh7 编排：建隐藏桌面 → 起 fixture → 建 UIA → 执行一组 L0 动作 → fresh snapshot 回读 → 销毁进程 → `CloseDesktop`。
  2. 断言全程输入桌面为 `Default`、用户前台窗口与光标不变、桌面句柄正确释放（无泄漏）。
  3. 注入故障：中途 kill fixture 进程，验证 session manager 能回收并报 `UNKNOWN_EFFECT` 而非挂起。
- **预期判据**：闭环成功 + 用户桌面零干扰 + 故障路径正确回收 → 候选路 (a) 可产品化。

---

# 附录 · 与现有安全模型的一致性

本方案不触动项目安全主轴。后台模式下的每个动作仍必须锚定"已观察且二次校验过的元素"（`snapshot_id + revision + node_id`），MCP schema 仍禁止裸坐标。隐藏桌面 + 纯 L0 只是把这条已有主路径搬进隔离桌面，寻址方式不变、投递通道不变（仍是 UIA Pattern，非合成输入），因此安全模型零改动。合成输入在后台模式下不可用属于"能力子集"，应通过结构化错误显式暴露（`DRIVER.BACKGROUND_INPUT_UNSUPPORTED`），而非静默降级。

**未闭环即诚实标注**：本文档所有 [待验证] 项均未在本环境实测（受"不抢前台/不改输入桌面"硬约束限制），已全部整理进 Part 3 待主 agent 串行执行的清单，并注明所需环境（提权 / 特定 SKU / macOS / Linux 真机）。
