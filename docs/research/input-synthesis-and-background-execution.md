# 输入合成与后台执行调研

> 调研日期：2026-09-11。回答两个问题：(1) 三端如何补上"模拟真人键鼠"的能力；(2) 这个工具能不能在后台跑、不跟用户抢输入，虚拟桌面/虚拟显示器分别能解决什么。
>
> **2026-09-11 更新：Windows 部分的 4 项关键假设已在本机实测，其中 2 项被推翻。** 实测脚本在 `tests/windows/`，逐条记录见 §6。凡未实测的（macOS、Linux、Wayland）一律仍标 **[待验证]**，不写成结论。

## 实测环境

| 项 | 值 |
|---|---|
| OS | Windows 11 家庭版 26200（10.0.26200） |
| 会话 | Session 1，交互式，**未提权**（Medium 完整性） |
| Python | 3.14.7 |
| 日期 | 2026-09-11 |

结论只在此环境成立。提权、Server SKU、RDP 会话、其他 Windows 版本均未覆盖。

## 实测结论速览

| # | 原假设 | 实测结果 |
|---|---|---|
| 1 | `SendInput` 键盘对 Raw Input 不可见 | ❌ **推翻**。虚拟键/扫描码对三通道全可见；**只有 `KEYEVENTF_UNICODE` 对 Raw Input 与 GetAsyncKeyState 完全不可见** |
| 2 | `SetThreadDesktop` 后 `SendInput` 作用于该 desktop | ❌ **推翻**。`SendInput` 只投递到**输入桌面**；非输入桌面一律 `ACCESS_DENIED`。**Windows 后台隔离方案不成立** |
| 3 | 隐藏桌面上应用可能不渲染 | ✅ 渲染正常，`PrintWindow(PW_RENDERFULLCONTENT)` 可取到与可见桌面几乎一致的内容 |
| 6 | 修饰键可能泄漏 | ✅ **确认且不自愈**。Ctrl 按下不释放会一直 latched，必须显式复位 |

---


## 0. 现状：已经有什么，缺口在哪

先把话说清楚——这个项目**不是没有合成输入**，三端都已经有了一条窄路径：

| 平台 | 已实现 | 代码位置 |
|---|---|---|
| Windows | `SendInput` 的 `KEYEVENTF_UNICODE` 打字；元素中心点左键单击（`MOUSEEVENTF_ABSOLUTE` 三事件） | `crates/aad-uia/src/windows.rs:444-575` |
| macOS | `CGEventKeyboardSetUnicodeString` + `postToPid` 分块投递；中心点左键 | `docs/architecture/macos-ax-driver.md` §116-140 |
| Linux | XTEST 文本输入（临时占用空闲 keycode 表达码点）；中心点左键 | `docs/architecture/linux-kde-x11-driver.md` §33-38, §154-161 |

也就是说，缺的不是"从零加输入"，而是这条路径**只覆盖了两种动作：打一串可见字符、点一个元素的中心**。真正的缺口是：

**A. 键盘表达力**。现在只能发 Unicode 码点。`KEYEVENTF_UNICODE` 合成的是 `VK_PACKET`，它**表达不了任何功能键与组合键**——Ctrl+C、Ctrl+Z、F5、Tab、Esc、方向键、Shift+End 选择、Alt+F4、Win 键，一个都发不出来。而"无法用界面元素描述的东西"里，最大的一类恰恰就是快捷键。

**实测追加了一个更严重的问题**：`KEYEVENTF_UNICODE` 不只是表达力弱，它**对 Raw Input 和 `GetAsyncKeyState` 两个通道完全不可见**——探针在这两条通道上收到 0 条记录，只有窗口消息看得到（`vkey=231`，即 `VK_PACKET`）。而虚拟键和扫描码在三条通道上都正常可见。也就是说，**项目当前唯一在用的键盘编码，恰好是三种编码里可见性最差的那一种**。详见 §6.1。


**B. 指针表达力**。只有"左键单击元素中心"。缺：右键（上下文菜单）、双击、中键、按住拖拽、悬停（触发 tooltip / hover 菜单）、滚轮（横竖）、精确到元素内某个相对位置（滑块、色板、画布、时间轴）。

**C. 修饰键与序列的组合**。Ctrl+点击多选、Shift+点击范围选择、拖拽时按住 Alt——这些是"修饰键状态"与"指针动作"的叠加，当前模型里没有位置放它。

**D. `pointer_click` 会抢前台**。`windows.rs:486` 明确调了 `SetForegroundWindow`。这不是 bug——点后台窗口会打到前面的窗口上——但它意味着现有实现在架构上就与"后台执行"互斥。这条是第二个问题的起点。

另外记一个已经踩过的坑，后面选型要用到：项目实测过**本机存在低级输入钩子时 `SendInput` 会整批拒绝**（返回 0 且 `GetLastError()` 为 0），且拆批投递会导致文字错乱（"one event at a time" 变成 "one eeeeeeeeeeeeeee"），所以现在的策略是整批失败、报 `DRIVER.INPUT_BLOCKED` 而什么都不写。这个教训对后面所有键盘方案都成立。

---

## 1. 结论摘要

1. **"模拟键鼠"不是一个能力，是五层不同的通道。** 从 UIA/AX/AT-SPI 语义动作，到窗口消息，到进程定向事件，到会话级合成输入，到内核虚拟 HID——越往下越像真人、越能穿透过滤，权限成本和不可观测性也越高。**该做的是把这五层显式建模成可声明、可探测、可拒绝的阶梯，而不是笼统加一个 `send_keys`。**

2. **Windows 的缺口最好补，成本最低。** 现有 `SendInput` 通道直接加 `KEYEVENTF_SCANCODE` + 虚拟键、加右键/双击/滚轮/拖拽即可，不需要新依赖、不需要新权限。这是投入产出比最高的一块。

3. **后台执行在三端的难度是 Linux < Windows << macOS，而且三者机制完全不同，不存在统一抽象。**
   - Windows：**原本认为 Desktop 对象是对口原语，实测证明不可行**——`SendInput` 只投递到当前"输入桌面"，隐藏桌面一律 `ACCESS_DENIED`（§6.2）。要在 Windows 上不抢输入，只剩"L0 语义动作"和"第二 Windows 会话"两条路。
   - Linux 最容易，因为"另开一个 display"本来就是常规操作（Xvfb / sway headless / mutter --headless）。**[待验证]**
   - macOS **没有公开 API 能做到**。目前唯一实证可行的路径依赖三个私有 SkyLight 符号，且已经出现过系统升级收紧的迹象。**[待验证]**


4. **虚拟显示器（IddCx 等）解决的是"没有物理显示器"，不解决"抢输入"。** 这两个需求经常被混为一谈。要"不抢输入"，需要的是**独立的输入队列/独立会话**，不是多一块屏。虚拟显示器只在无头服务器、或需要固定分辨率/多屏布局时才是答案。

5. **"禁坐标"和"禁键鼠"是两件事，不该一起禁。** 项目的安全主轴是"任何写操作必须引用一个真实观察过的元素"。这条约束**完全可以保留**——键盘锚定"当前焦点元素"，拖拽锚定"起点元素+终点元素"，滚动锚定容器元素。真正要守住的是**寻址方式**（不许 AI 凭空给坐标），而不是**投递通道**（用不用合成输入）。详见 §4。

---

# Part A：模拟键鼠

## 2.1 先把分层讲清楚

任何一次"操作界面"，都可以走下面五层之一。这个分层是后面所有选型的基础：

```
L0  语义动作      UIA Pattern / AX Action / AT-SPI Action
     ↑ 可验证、不动光标、跨桌面有效、可能不被支持
L1  窗口消息      PostMessage / SendMessage (WM_KEYDOWN, WM_LBUTTONDOWN)
L2  进程定向事件   CGEvent.postToPid / SLEventPostToPid（macOS 独有）
L3  会话合成输入   SendInput / CGEventPost / XTEST / libei / wlr-virtual-*
L4  内核虚拟 HID   uinput / Interception / ViGEmBus / IOHIDUserDevice
     ↓ 像真人、穿透过滤、需要驱动或 root、不可观测、易被误判为外挂
```

几个必须记住的性质：

- **L0 不需要焦点，也不移动光标**，而且能在隐藏桌面/遮挡窗口上工作。项目现在的 `invoke`/`set_value` 就在这一层，这是它比"坐标点击类工具"更可靠的根本原因。
- **L3 是"会话级"的**：它注入的是整个桌面会话的输入流，所以必然会移动光标、必然改变焦点。这是它天然与"后台"冲突的原因。
- **越往下，"确认它生效了"越难**。L0 可以回读 Pattern 值；L4 只能靠重新观察界面。项目已有的 `postcondition` 机制在这里价值翻倍。

**能用 L0 解决的，永远不要下沉。** 补键鼠的目的不是替代 L0，是覆盖 L0 表达不了的那部分。

## 2.2 Windows

### 2.2.1 SendInput：现有通道，只用了一小半

`SendInput` 是 Windows 上的标准答案，也是项目已经在用的。它的关键性质在 MSDN 上写得很明确：一次调用内的事件**不会与其他来源的输入交错**<sup>[1]</sup>——这正是现在整批投递的依据，要保留。

要补的是三样：

**(1) 虚拟键与扫描码。** 现在只用 `KEYEVENTF_UNICODE`。MSDN 明确：该标志合成的是 `VK_PACKET`，`wVk` 必须为 0<sup>[2]</sup>——所以它**结构上就无法表达功能键和组合键**。补法是走 `wVk`（虚拟键码）或 `wScan + KEYEVENTF_SCANCODE`（扫描码）。两者的区别值得写进设计：

| | 虚拟键 `wVk` | 扫描码 `wScan + KEYEVENTF_SCANCODE` |
|---|---|---|
| 含义 | 逻辑键（VK_RETURN 恒为 0x0D） | 物理键位（不随键盘布局变） |
| 适合 | 常规应用的快捷键 | 游戏 WASD、需要"按键物理位置"的场景 |
| 依据 | 微软文档明说：桌面应用通常用虚拟键更合适，但需要"不管布局、就是这个物理键"时用扫描码，例如游戏的 WASD<sup>[3]</sup> | 同左 |

建议：**默认虚拟键，把扫描码作为显式可选项**，不要默认走扫描码——扫描码在 AZERTY 布局下按 "W" 的位置发出的是别的字符，对普通应用是错的。

**(2) 指针动作补全。** `MOUSEEVENTF_RIGHTDOWN/UP`、`MIDDLEDOWN/UP`、`WHEEL`（`mouseData` 传 `WHEEL_DELTA` 倍数）、`HWHEEL`。双击靠两次单击加间隔（间隔要小于 `GetDoubleClickTime()`）。拖拽是 `down → 多次 move → up` 的序列。

**(3) 修饰键状态。** 组合键必须是 `Ctrl down → C down → C up → Ctrl up` 的完整序列，且**必须整批提交**——这正是现有 `deliver_atomically` 的设计意图，直接复用。要特别小心的是**修饰键泄漏**：如果批次中途失败，Ctrl 可能停在按下状态，之后用户的一切输入都会变成组合键。这比"没打出字"严重得多。建议：**任何按下修饰键的批次，都必须在同一批里包含释放**，且失败时以 `effect=unknown` 上报并提示可能需要复位。

### 2.2.2 UIPI：注定跨不过去的墙

`SendInput` 受 UIPI 约束，微软文档原文："仅允许应用程序将输入注入到完整性级别相等或更低级别的应用程序"<sup>[1][4]</sup>。这意味着**普通权限运行的 `aad` 无法操作以管理员身份运行的程序**——不只是输入，UIA 枚举也会拿到空树<sup>[5]</sup>。

绕过方式只有两条，都有代价：

- **同权限运行**：让 `aad` 也提权。简单，但把整个工具的权限面抬高了。
- **uiAccess 清单**：设置 `uiAccess=true` 可绕过 UIPI，这是给屏幕阅读器等辅助技术设计的<sup>[6]</sup>。代价是**必须数字签名 + 必须安装在受保护目录（Program Files）**，否则系统拒绝提权<sup>[7]</sup>。且实践中有 MSIX 协议激活拒绝 uiAccess exe（0x300D）的报告<sup>[8]</sup>。

**建议：不要绕。** 把它做成一个能被 `probe` 探测、能被 driver 明确报错的**边界**：检测到目标窗口完整性级别高于自身时，直接返回结构化错误并说明"目标以管理员运行，请以同等权限启动，或改用不需要注入的语义动作"。这比让用户面对一次静默失败要好得多——微软自家的 Power Automate 也把 UIPI 列为独立的故障排查条目<sup>[9]</sup>。

同样属于硬边界的还有：**锁屏、UAC 提示、登录界面**运行在独立的 `Winlogon` desktop 上，任何用户态注入都到不了<sup>[9][10]</sup>。这个应该 fail fast 而不是超时。

### 2.2.3 键盘合成对 Raw Input 的可见性（已实测，原假设被推翻）

有一条流传较广、也有一手描述的说法：`SendInput` 的**键盘**事件不经过 `KBDCLASS` 内核驱动，因此只读 Raw Input / DirectInput / `GetAsyncKeyState` 的消费者（主要是游戏引擎）看不到<sup>[11]</sup>。

**本机实测结果与这条说法相反**，而且分化点不在"键盘 vs 鼠标"，在**键盘编码方式**：

| 发送方式 | Raw Input | GetAsyncKeyState | 窗口消息 |
|---|:---:|:---:|:---:|
| 虚拟键 `wVk`（A、F1、Ctrl+A） | ✅ | ✅ | ✅ |
| 扫描码 `KEYEVENTF_SCANCODE` | ✅ | ✅ | ✅ |
| **`KEYEVENTF_UNICODE`（项目现用）** | ❌ **0 条** | ❌ **0 条** | ✅ |
| 鼠标（左键/右键/滚轮） | ✅ | — | ✅ |

所以真正的结论是：**`SendInput` 本身对 Raw Input 是可见的，但 `KEYEVENTF_UNICODE` 不是。** 这很合理——`VK_PACKET` 本来就是一个"投递字符"而非"模拟按键"的机制，它没有对应的物理键，也就没有扫描码可以送进 Raw Input 流。

对本项目的直接影响：现有 `type_text` 走 `KEYEVENTF_UNICODE`，因此**在只读 Raw Input 的应用（游戏、部分自绘 UI、DirectInput 消费者）里，它打的字不会被看到**。补虚拟键/扫描码路径不仅是为了组合键，也顺带修掉这个可见性缺口。

证据与复现见 §6.1。


### 2.2.4 其他方案：都不建议作为主路径

**PostMessage / SendMessage（L1）**。诱惑很大：不需要焦点、不移动光标、能绕 UIPI（HWND 定向而非全局注入）<sup>[13]</sup>。但不可靠得多：

- WinUI 3 / UWP 的 XAML 控件是"无窗口"的，没有子 HWND，投递过去的 `WM_CHAR` 没人接<sup>[13]</sup>；
- 现代框架（WPF/WinForms）通常依赖完整的消息序列和正确的 `lParam` 键状态编码，只发 `WM_KEYDOWN` 常常无效<sup>[14]</sup>；
- Chromium/游戏根本不读消息队列。

**结论：可以作为特定应用的"加速通道"，绝不能作为通用后备。** 它的失败方式是"静默无反应"，这是最糟糕的失败方式。

**WinRT InputInjector**（`Windows.UI.Input.Preview.Injection`）。提供鼠标、键盘、**触摸、笔**的注入<sup>[15]</sup>。相对 `SendInput` 的独特价值是**触摸和笔**——如果将来要覆盖触屏应用的手势，这是唯一的公开路径。但它同样受 UIPI 约束，且在远程会话中可能"接受了但没到达目标应用"<sup>[16]</sup>。**建议：不进 P0/P1，记为触摸场景的备选。**

**驱动级（L4）：Interception / ViGEmBus。** 能穿透绝大多数过滤，在游戏、UAC 对话框、登录界面都能工作<sup>[17]</sup>。代价极高，而且是**方向性错误**：需要安装内核驱动、需要 SYSTEM 权限、多数高级反作弊（Vanguard、部分 EAC 版本）在检测到 Interception 驱动加载时直接拒绝启动<sup>[18]</sup>。更重要的是——一个"以观察为前提、拒绝盲点击"的工具，装一个专门用来伪装成物理硬件的内核驱动，在定位上是自相矛盾的。

**明确建议：不做 L4。** 如果某天真有需求，做成第三方插件由用户自行承担，不进主干。

## 2.3 macOS

### 2.3.1 三条通道，各有各的死法

**`CGEventPost`（kCGHIDEventTap）**：投进 HID 事件流，和物理鼠标同一条路。所以它**会移动真实光标**——WindowServer 看到 (342,198) 的点击，就把指针更新到 (342,198)，因为真人点击就是这样<sup>[19]</sup>。兼容性最好，但与后台执行完全互斥。

**`CGEvent.postToPid`（项目现用）**：定向到某个进程，不移动光标。问题是**Chrome / Electron 会在 renderer IPC 边界把它当作不可信事件静默丢弃**——因为它缺少真实 HID 管线会附加的那些字段（`mouseEventSubtype` 字节、`clickState` 计数、私有 SPI 设置的窗口局部坐标戳）<sup>[19]</sup>。考虑到"大部分应用是 Electron"（Slack、VS Code、Discord、Notion），这个缺口很大。另有报告称 **macOS 26 进一步收紧了非前台窗口的 `postToPid` 投递语义**，Safari WebKit 内容即使授予 Accessibility 也收不到<sup>[20]</sup> **[待验证]**。

**SkyLight 私有框架**：`SLEventPostToPid` 走的是另一条代码路径（一个 auth-signed 的 SkyLight 通道，完全绕开 `IOHIDPostEvent`），Chrome 的过滤器**接受**从这条通道来的事件<sup>[19]</sup>。配套还需要两个符号：

- `SLPSPostEventRecordTo`——借用 yabai 的 focus-without-raise 手法，翻转目标 App 的 AppKit active 状态而**不调用** `SLPSSetFrontProcessWithOptions`（后者会把窗口提到前面并触发切 Space）<sup>[19]</sup>；
- `_AXObserverAddNotificationAndCheckRemote`——Electron 应用窗口被遮挡时 Blink 会暂停 AX 树更新，公开的 `AXObserverAddNotification` 不会把观察者标记为 remote-aware，这个私有变体会<sup>[19]</sup>。

这套组合已经由开源的 `cua-driver` 实证可用<sup>[19]</sup>。

**但代价要说清楚**：三个符号全部**未文档化、部分不在任何 Apple 头文件中**，靠 `dlopen`/`dlsym` 拿到<sup>[19]</sup>。这意味着 (a) 任何一次系统更新都可能失效；(b) 无法上架 App Store；(c) 出问题时没有任何官方支持。

**建议：做成显式可选的第二档能力**，默认不启用，`probe` 明确报告"当前 macOS 版本上此通道是否可用"，失效时优雅降级到"激活前台再操作"，而不是整个功能崩掉。

### 2.3.2 两个绕不过去的硬边界

即便上了 SkyLight，`cua-driver` 作者也点名了两个不可解的：

- **Chromium 把合成的右键在网页内容上强制降级成左键**。renderer IPC 过滤器会丢掉非 HID-tap 路径上的右键 subtype。AX 可寻址的目标（链接、按钮、工具栏项）走 AX action 没问题，纯网页内容只能左键<sup>[19]</sup>。
- **Canvas 类应用（Blender GHOST、Unity、游戏）完全过滤 per-pid 路由**，它们的事件循环只认带前导 `mouseMoved` 的 `cghidEventTap`，必须短暂前台激活——"不抢焦点"的承诺在这一类应用上直接破功<sup>[19]</sup>。

### 2.3.3 权限模型

macOS 的 TCC 把权限拆成三份，必须分别申请<sup>[21]</sup>：

| 权限 | 控制什么 |
|---|---|
| **PostEvent** | 能否 post UI 事件（即 `CGEvent.post`） |
| **ListenEvent** | 能否全局监听事件（录制需要） |
| **ScreenCapture** | 能否截屏 |

要点：**签名身份必须稳定**，否则每次重新签名都要用户重新授权（项目文档 `runtime.md:132` 已记录这一点）。另外 `EnableSecureEventInput`——密码输入场景下应用会启用它，此时合成输入会被系统吞掉<sup>[22]</sup>。这与项目现有的"密码框可写不可读"策略需要对齐：`set_value` 走 AX 不受影响，但合成键盘输入在 secure input 期间会静默失败，**必须能检测并明确报错**。

## 2.4 Linux

Linux 是三端里碎片化最严重的，而且**没有一条路能覆盖所有环境**。

### 2.4.1 X11：XTEST，成熟且够用

项目现在走的路。成熟、无权限提示、任何窗口管理器都能用。xdotool 作者的原话是："在 X11 上，xdotool 几乎能在任何窗口管理器下工作"<sup>[23]</sup>。

现有实现已经解决了一个很硬的问题：**现有键盘映射无法表达的码点，分别占用不同的空闲非 modifier keycode，全部映射一次性提交并同步后才派发，结束时恢复映射**（`linux-kde-x11-driver.md:156-158`）。这套机制补功能键/组合键时可以直接复用——功能键反而更简单，因为它们本来就在 keymap 里。

### 2.4.2 Wayland：一片沼泽

xdotool 作者 2025 年 11 月的实测文章把现状讲得很透，值得原样引用几条<sup>[23]</sup>：

- **GNOME 48**：Xwayland 可以用 XTEST，但每隔几分钟弹一次几乎没有上下文的"允许远程交互"提示。也可以走 XDG Portal 的 RemoteDesktop session + libei，**但仍然弹提示，且尽管 portal API 文档里写了持久授权，实际找不到永久授权的办法**。
- **KDE**：XTEST 表现类似，弹"远程控制请求"。
- **wlroots 系**（sway / Hyprland / river / Wayfire）：支持 `zwp_virtual_keyboard_v1` + `zwlr_virtual_pointer_v1` 这类协议扩展<sup>[24][25]</sup>，可以直接绑，无提示。
- 作者的总结是："在 Wayland 上，每个 compositor 都需要自己的实现"<sup>[23]</sup>。

补充几条来自其他项目的现状：

- KDE Plasma 的**指针**支持还在补，有项目记录"鼠标支持预计在 KDE 6.5 通过 `pointer-warp-v1` 落地"<sup>[26]</sup> **[待验证]**；
- `cua` 项目在 GNOME/Wayland 上的诊断输出很有代表性：GNOME 不广告 `zwlr_virtual_pointer`，未编译 libei/portal 支持的构建**点击和按键完全不会送达**，但窗口枚举和截图不受影响<sup>[27]</sup>。这说明**输入能力与观察能力必须分别探测**，不能用一个 `linux: ok` 概括。

### 2.4.3 uinput：跨 compositor，但代价明确

`/dev/uinput` 在内核层创建虚拟键鼠设备，不受 Wayland 限制<sup>[28]</sup>。代价：

- **权限**：需要 root，或把用户加进 `input` 组 + udev 规则<sup>[28][29]</sup>。这是一个持久的系统级权限变更，不是运行时授权。
- **发的是键码不是符号**。xdotool 作者点明了这一层难度：要发出符号 'a'，得先知道哪个 keycode 产生 'a'，也就得拿到键盘映射，而拿映射本身又有三条互不相同的路<sup>[23]</sup>。实际后果是 `ydotool` **不识别自定义键盘布局**，字符会按 US 布局打出<sup>[30]</sup>。
- **多显示器绝对定位有坑**：单个 uinput 绝对轴指针无法表示非矩形的多屏并集<sup>[31]</sup>。

**建议：uinput 作为 Wayland 上的最后手段**，且必须让用户显式选择、显式知道权限代价，不做静默 fallback。

### 2.4.4 Linux 选型建议

按 compositor 分档，`probe` 必须能报出当前落在哪一档：

| 环境 | 首选通道 | 提示 | 备注 |
|---|---|---|---|
| X11（任意 WM） | XTEST | 无 | 现有实现，最成熟 |
| wlroots 系 | `zwp_virtual_keyboard_v1` + `zwlr_virtual_pointer_v1` | 无 | 需 compositor 广告该协议 |
| GNOME / KDE Wayland | XDG RemoteDesktop portal + libei | **每次弹窗，无持久授权** | 交互式可接受，无人值守基本不可用 |
| 以上都不可用 | uinput | 无（但需预先配权限） | 键码而非符号，布局问题 |

**一个重要判断：GNOME/KDE Wayland 上的"无人值守自动化"目前基本不成立**，因为拿不到持久授权。这不是实现问题，是上游现状。应该如实写进能力声明，而不是靠重试或绕路掩盖。

## 2.5 三端横向对比

| 维度 | Windows | macOS | Linux |
|---|---|---|---|
| 主通道 | `SendInput` | `CGEvent.postToPid` / SkyLight | XTEST / wlr-virtual-* / libei / uinput |
| 权限门槛 | 无（同完整性级别内） | TCC 三权分立，需稳定签名 | X11 无；Wayland 视 compositor；uinput 需 root/input 组 |
| 后台友好 | **不友好**（`SendInput` 绑定输入桌面，见 §3.2） | 需私有 API **[待验证]** | 需独立 display **[待验证]** |
| 硬边界 | 高完整性进程、锁屏/UAC、`KEYEVENTF_UNICODE` 的 Raw Input 盲区 | Chromium 右键降级、Canvas 类应用、Secure Input | Wayland 碎片化、GNOME/KDE 无持久授权 |
| 建议优先级 | **P0**（成本最低、收益最大） | P2（依赖私有 API，需先定策略） | P1（X11 + wlroots 先做，portal 后做） |

---

# Part B：后台执行与虚拟桌面

## 3.1 先拆需求：三件不同的事

"能不能后台跑、不跟用户抢输入"这句话里其实混了三个诉求，技术路径完全不同：

| # | 诉求 | 本质需要 | 虚拟显示器能解决吗 |
|---|---|---|---|
| **a** | 不移动用户光标、不抢焦点，但同一个桌面 | **进程定向的事件投递** | ❌ 不能 |
| **b** | 完全隔离，用户看不见也不受影响 | **独立的输入队列 / 独立会话** | ❌ 不能（但常配合使用） |
| **c** | 没有物理显示器也能跑 | **虚拟显示输出** | ✅ 正是它的用途 |

**关键结论：虚拟显示器（IddCx / Parsec VDD 之类）解决的是 c，不解决 a 和 b。** 它的官方定位是"为虚拟桌面环境创建虚拟监视器"、"网络串流远程显示"、"无头服务器"<sup>[32]</sup>。给机器插一块虚拟屏，输入照样是那一套全局输入流，照样抢光标。别指望它。

## 3.2 Windows：Desktop 对象——实测证明此路不通

> **本节结论已被实测推翻。** 原本认为 Desktop 对象是 Windows 后台隔离的对口原语，实测证明 `SendInput` 无法投递到非输入桌面。保留推理过程是因为它解释了为什么两个开源项目会给出矛盾的说法，也说明了为什么"看起来很对"的方案需要实测。

### 3.2.1 机制（这部分仍然成立）

Windows 的 GUI 层次是 `Session → Window Station → Desktop`。**Desktop 是一个 UI 消息处理域，它决定哪些窗口能接收鼠标点击和键盘输入**<sup>[33]</sup>。同一个 Window Station（`WinSta0`）下可以创建多个 desktop，消息只能在同一 desktop 内的进程间传递<sup>[34]</sup>。

隔离本身是真的，实测确认可用的部分：

```
CreateDesktopW("AadAutomation")                     // ✅ 成功
CreateProcessW(..., si.lpDesktop = "AadAutomation") // ✅ 进程确实起在该桌面
EnumDesktopWindows(hDesktop, ...)                   // ✅ 能枚举到该桌面的窗口
PrintWindow(hwnd, PW_RENDERFULLCONTENT)             // ✅ 能截到真实内容（§6.3）
SetThreadDesktop(hDesktop)                          // ✅ 返回成功
SendInput(...)                                      // ❌ 返回 0，ERROR_ACCESS_DENIED
```

**除了输入，全都能用。** 偏偏输入是关键的那一环。

### 3.2.2 两个开源实证的矛盾，以及实测给出的裁决

**实证 A（winforms-mcp）**：`HEADLESS=true` 时用 `CreateDesktop` 建隐藏桌面，UIA 全部操作正常，`PrintWindow` 能截到内容。**但它明确说 `send_keys` / `drag_drop` / 双击到不了隐藏桌面**，因为"输入模拟针对活动桌面的输入队列"<sup>[35]</sup>。

**实证 B（Roblox sandbox）**：同样 `CreateDesktop`，但**它说 SendInput 完全可用**——WASD、跳跃、点击全都工作<sup>[36]</sup>。

我原先的推断是：差别在于实证 B 有专门线程先调 `SetThreadDesktop` 再 `SendInput`，所以 `SendInput` 应该跟随调用线程的 desktop。

**实测结论：这个推断是错的，实证 A 是对的。**

`SendInput` 跟随的是**输入桌面（input desktop）**——即当前正在显示、接收物理输入的那个 desktop——而与调用线程关联的 desktop 无关。决定性对照实验（§6.2）只改变"隐藏桌面是否为输入桌面"这一个变量：

| 条件 | `SendInput` | 探针收到 |
|---|---|---|
| 隐藏桌面**不是**输入桌面 | `sent=0`，`ACCESS_DENIED` | 0 条 |
| `SwitchDesktop` 后**成为**输入桌面 | `sent=2`，无错误 | 完整按键链路 |

而 `SwitchDesktop` 会把屏幕真的切过去——用户当场黑屏。**这与"不抢输入"的目标直接矛盾**，所以它不能作为后台方案，只能用于调试时"看一眼"。

至于实证 B 为什么能工作：它面向的是游戏场景，很可能实际运行时那个桌面就是输入桌面（或用了别的注入方式）。**不要照抄它的结论。**

### 3.2.3 排除了其他解释

`ACCESS_DENIED` 有多种可能成因，逐一排除后才敢下结论（§6.2）：

| 假设 | 处理 | 结果 |
|---|---|---|
| 桌面句柄权限不足 | 用显式 `DESKTOP_*` 权限位取代 `GENERIC_ALL` | 仍失败 |
| 句柄本身有问题 | `OpenDesktopW` 按名重新打开 | 仍失败 |
| 线程切换过去的方式有问题 | **让进程"出生"就在隐藏桌面上**（`lpDesktop`，全程不调 `SetThreadDesktop`） | 仍失败 |
| 本进程有窗口/钩子污染 | 同上，全新进程 | 仍失败 |
| 对照：同一进程在 `Default` 桌面 | — | **成功** |

三条独立路径全部 `ACCESS_DENIED`，同一进程在输入桌面上成功，且 `OpenInputDesktop` 全程报告 `Default`。指向唯一解释：**投递目标由输入桌面决定**。

### 3.2.4 仍然可用的部分

Desktop 对象没有全盘失效，**只是不能用合成输入**。以下经实测仍成立：

- **窗口枚举**：`EnumDesktopWindows` 正常。
- **截图**：`PrintWindow(PW_RENDERFULLCONTENT)` 在隐藏桌面上取到的内容与可见桌面几乎一致（§6.3），**原先担心的"不渲染"没有发生**。
- **UIA 语义动作（L0）**：winforms-mcp 报告 UIA 全部操作正常<sup>[35]</sup> **[待验证]** —— 本轮未实测 UIA，但它不依赖输入队列，原理上应当可行。

**这指向一个替代设计**：隐藏桌面 + 纯 L0 语义动作（`Invoke`/`SetValue`/`Toggle`），完全不用合成输入。对本项目而言这个组合并不牵强——L0 本来就是主路径，合成输入只是补充。代价是隐藏桌面上**不能**用键鼠兜底，L0 表达不了的动作在后台模式下就是做不到。

### 3.2.5 其他 Windows 路线

**第二 Windows 会话（UiPath PiP - Child Session）**。UiPath 的成熟做法：以画中画模式启动流程时**新开一个 Windows 会话**，机器人在隔离会话里跑，用户继续用自己的桌面<sup>[38][39]</sup>。UiPath 同时提供两个版本：Child Session（独立会话）和 Virtual Desktop（同一会话、虚拟桌面）<sup>[38]</sup>。

值得注意：**UiPath 的 Virtual Desktop 版本本质就是 §3.2.1 的 Desktop 对象，而实测表明这条路上合成输入不可用**——这或许正是 UiPath 同时提供 Child Session 版本的原因。独立会话有自己的输入桌面，不受本节限制。

代价：需要 Windows 凭据认证新会话<sup>[40]</sup>；且 **Windows 客户端版（Win10/11 Pro/Home）不支持真正的多用户并发**<sup>[41]</sup>。对一个要装在开发者个人机器上的工具，这条路太重。**但在 Windows 上它现在是唯一能同时满足"完全隔离"和"可用合成输入"的路径。**


**RDP + tscon。** 经典的运维技巧：RDP 断开后 UI 自动化会失效（会话进入 disconnected 状态、没有可交互桌面），用 `tscon <session> /dest:console` 把会话重定向回控制台可以保持桌面活跃<sup>[42][43]</sup>。相关的还有 `RemoteDesktop_SuppressWhenMinimized=2` 注册表项，解决 RDP 窗口最小化后自动化挂起<sup>[44]</sup>。

**这些是部署技巧，不是产品能力**，但值得写进"已知环境限制"——因为用户在 RDP 环境下遇到问题时，需要知道这不是工具的 bug。微软自己的 Power Automate 文档就把"最小化的 RDP 会话"列为 UIPI 故障的独立成因<sup>[9]</sup>。

**Session 0 隔离。** 一个反面教训：把 driver 做成 Windows 服务跑在 Session 0，**将无法驱动用户桌面**——`SendInput`、UIA `FromHandle` 对跨会话 HWND 全部失效<sup>[45]</sup>。如果将来考虑"开机自启的后台服务"形态，这条是硬墙。

## 3.3 macOS：最难，且没有好答案

**同一个 login session 内，没有任何公开 API 能做到"保持用户焦点不变的同时向后台应用发按键"。** 这是一个开源项目在尝试后明确写下的结论并回退了整个分支<sup>[46]</sup>。

现实选项只有三个，都不理想：

1. **私有 SkyLight 路径**（§2.3.1）。能达成需求 a——光标不动、焦点不变、不跨 Space<sup>[19]</sup>。但依赖未文档化符号，且对 Canvas 类应用破功。
2. **独立 login session**：用第二个用户账号 + `launchctl asuser` 起一套独立的 GUI 会话<sup>[47]</sup>。这是真隔离，但代价是快速用户切换的一整套限制，且有报告称从锁屏点"切换用户"时 `launchctl asuser` 的 bootstrap context 行为异常<sup>[48]</sup>。另外 macOS 的图形会话与显示设备绑定，后台用户运行图形应用受限<sup>[49]</sup> **[待验证]**。
3. **VM**：`vz` 框架跑 macOS 虚拟机。最干净，也最重。

**建议：macOS 上先不承诺后台能力。** 把 §2.3 的输入补全做扎实（前台模式），后台作为独立课题，等 SkyLight 路径在多个系统版本上被验证稳定后再评估。

## 3.4 Linux：最容易

因为"另开一个 display"在 Linux 上本来就是常规操作，需求 b 和 c 一起就解决了：

| 方案 | 形态 | 适用 |
|---|---|---|
| **Xvfb** | 虚拟 X server，无显示输出 | X11 应用，最成熟。项目文档里已经用过（`linux-kde-x11-driver.md:223` 提到"私有 Xvfb + 私有 AT-SPI bus"验证过 type_text） |
| **sway headless** | wlroots compositor，`WLR_BACKENDS=headless`；支持 `swaymsg create_output` 建 HEADLESS-N 输出<sup>[50]</sup> | Wayland 多窗口，且 wlr 虚拟输入协议可用——**这是 Wayland 上最顺的组合** |
| **mutter --headless --virtual-monitor** | GNOME 的无头模式<sup>[51]</sup> | 需要 GNOME 语义的场景；输入走 RemoteDesktop |
| **cage** | 单应用 kiosk | 只跑一个程序时最简单 |
| **weston --headless** | 参考实现 | 无 wlroots 协议，工具链少 |

有一份对比总结推荐：**多窗口测试用 sway headless，单应用用 cage**<sup>[52]</sup>。

同样的限制成立：**只能操作在这个 display 上启动的进程**，不能接管用户现有窗口。

好消息是，项目**已经有过 Xvfb + 私有 AT-SPI bus 的实践**（用于 KCalc 隔离验证），这条路的工程经验已经在仓库里了，扩展成产品能力的增量不大。

## 3.5 后台能力横向对比

| | Windows | macOS | Linux |
|---|---|---|---|
| **a. 不抢焦点，同桌面** | 部分（L0 语义动作天然满足；合成输入不行） | 仅私有 API **[待验证]** | 部分（L0 满足）**[待验证]** |
| **b. 完全隔离** | ⚠️ Desktop 对象**仅观察可用，合成输入不可用**（实测）；真隔离需第二会话 | ❌ 需第二用户或 VM | ✅ 独立 display **[待验证]** |
| **c. 无物理显示器** | ✅ IddCx 虚拟显示器 | 有限 | ✅ Xvfb / headless compositor |
| 能操作用户现有窗口 | ❌（b 模式下） | — | ❌（b 模式下） |
| 改造成本 | 观察侧低；**输入侧此路不通** | 高 | 低（已有 Xvfb 实践） |

**修正后的一句话**：Windows 上的隐藏桌面**只能做"看"，不能做"合成输入"**——要在隐藏桌面里操作，只能靠 L0 语义动作，或者上更重的第二会话方案。Linux 因为独立 display 自带输入通道，仍是三端里最顺的 **[待验证]**。三端共同的边界不变：后台模式下只能"agent 自己开程序自己用"，接管用户现有窗口在任何平台都做不到。


---

## 4. 与现有安全模型的关系

这是我认为最需要提前想清楚的一块，因为处理不好会毁掉项目现在最有价值的东西。

### 4.1 "禁坐标"禁的是寻址，不是通道

README 里写得很硬：MCP 工具 schema **不接受任何** `x`/`y`/`point`/`coordinates`/`position` 字段，并有测试守着。这条规则的目的是——**AI 不能绕过观察直接点坐标**。

但要注意，现有的 `pointer_click` **本来就在用坐标**（`click_point(node.bounds)` 算出元素中心，再转成 `MOUSEEVENTF_ABSOLUTE`）。区别在于：**坐标是从一个被观察、被二次校验过的元素推导出来的，而不是调用方给的。**

所以补键鼠能力时，要守的不变量是：

> **每一个合成输入动作，都必须能追溯到一个通过 `snapshot:revision:node` 校验过的元素。**

而不是"不许用合成输入"。

### 4.2 各类新动作的锚定方式建议

| 新动作 | 锚定什么 | 备注 |
|---|---|---|
| 按键 / 组合键 | **焦点元素**（先 focus 一个校验过的元素，再发键） | 与现有 `type_text` 完全同构，改动最小 |
| 右键 / 双击 / 中键 | 元素（同 `pointer_click`） | 直接扩展现有实现 |
| 滚轮 | **可滚动的容器元素** | 应校验目标确实可滚动，否则报 `ACTION_UNSUPPORTED` |
| 悬停 | 元素 | 需要"停留时长"参数 |
| 拖拽 | **起点元素 + 终点元素** | 两端都要校验；中间路径可以是插值 |
| 元素内相对位置 | 元素 + **归一化比例**（0.0–1.0） | 给滑块/画布用。用比例而非像素，DPI 变化时仍成立 |
| 全局快捷键（Win/Cmd 键等） | **不锚定元素，锚定"意图"** | 这是唯一的例外，见下 |

**全局快捷键是唯一真正的例外**，因为它按定义不属于任何元素（Win+D、Cmd+Space、Alt+Tab）。建议：单独一个 action 类型、单独一条 policy 开关、默认关闭，并且在 workflow descriptor 里必须显式声明。**不要为了它把整套锚定机制放松。**

> 实现状态（Windows）：已按此落地为 `control_global_key` / 协议操作 `global_key`，独立的 `--allow-global-keys` 开关（还需 `--allow-input`），默认关闭，只在授权时出现在 `control_capabilities`；复用原子 chord 与修饰键清理，拒绝 Ctrl+Alt+Del、Win+L 和安全桌面，成功时 commit_state 为 `unknown`。详见 `docs/control-sessions.md`。保存的工作流步骤尚不能包含它（未做 descriptor 显式声明）。

### 4.3 effect 语义要更保守

项目现有的 `effect` 三态（`not_applied` / `contextual` / `unknown`）在这里要用得更严：

- 合成输入**成功派发 ≠ 生效**。这一点现有文档已经反复强调，键鼠只会更严重——一次拖拽中途失败，界面可能停在一个中间状态。
- **修饰键泄漏是新增的失败模式，且已实测确认不会自愈**（§6.4）：发送 Ctrl 按下而不发释放，2 秒后 `GetAsyncKeyState` 与 `GetKeyState` 仍报告按下，系统**没有**任何自动复位。后果会溢出到本次操作之外，污染用户后续的一切输入。这需要一个现有错误模型里没有的概念：**"失败的影响超出了本次动作的范围"**。建议：
  - 任何按下修饰键的批次，必须在同一批内包含释放；
  - driver 侧维护"当前按下的修饰键"影子状态，批次异常时**主动补发释放**；
  - 新增 detail 字段（如 `residual_state`）明确上报残留，而不是塞进 `effect`。
- `postcondition` 机制在这里价值翻倍。补键鼠的同时，应该把 GUI 里的 "check it worked" 勾选项扩展到新动作类型。

### 4.4 probe 要能分通道报告

现在 `probe` 报的是环境整体状态。补输入能力后，**必须细分到通道**，因为同一台机器上"能观察"和"能输入"经常不一致（GNOME/Wayland 就是典型：窗口枚举和截图正常，输入完全不可达<sup>[27]</sup>）。

建议 probe 输出至少覆盖：

- 每个输入通道是否可用（L0/L1/L3/L4 分别报）
- Windows：当前进程完整性级别、是否存在低级输入钩子（现有 `INPUT_BLOCKED` 的成因，能提前探测最好）
- macOS：PostEvent / ListenEvent / ScreenCapture 三项 TCC 分别的授权状态；SkyLight 符号是否可解析
- Linux：会话类型（X11/Wayland）、compositor、哪些虚拟输入协议被广告、portal 是否可用、uinput 是否可写
- 后台能力：是否支持独立桌面/display

---

## 5. 落地路线建议

按性价比排序。每一档都能独立交付价值，不必等下一档。

### P0：Windows 键鼠表达力补全（成本最低、收益最大）

不需要新依赖、不需要新权限、不改架构，纯粹把现有 `SendInput` 通道用满。实测已确认这条通道在本机完全可用（三通道可见、组合键正常、鼠标全动作正常）。

- 虚拟键 + 扫描码两种键盘模式；功能键、组合键、修饰键序列
- 右键 / 双击 / 中键 / 滚轮（横竖）/ 悬停 / 拖拽
- 元素内归一化相对位置
- **修饰键泄漏防护**（实测确认不自愈，见 §6.4）：同批必配对释放 + driver 侧影子状态 + 异常时主动补发释放 + 残留状态上报
- **`type_text` 增加"按键模式"选项**：现有 `KEYEVENTF_UNICODE` 对 Raw Input 不可见（§6.1），面向游戏/自绘 UI 时需改走虚拟键或扫描码
- UIPI 边界检测：目标完整性级别高于自身时 fail fast 并给出可执行建议
- 锁屏 / UAC desktop 检测：fail fast，不要等超时

**验证方式**：`tests/windows/input_channel_probe.py` 已经是这样一个"分通道回读"的探针，可以直接接进 fixture 体系，作为回归测试的基础——它按通道记录收到了什么，比"没报错"强得多。

### P1：Linux 键鼠补全（X11 + wlroots）+ 通道探测

- XTEST 补功能键与组合键（复用现有 keycode 临时映射机制）
- wlroots 虚拟输入协议后端（`zwp_virtual_keyboard_v1` + `zwlr_virtual_pointer_v1`）
- probe 细分到通道，GNOME/KDE Wayland 如实报告"需要每次授权、不支持无人值守"

### ~~P1：Windows 后台隔离（Desktop 对象）~~ —— 已实测否决

`SendInput` 无法投递到非输入桌面（§6.2），这条路的输入侧走不通。**不要再按这个方向开工。**

替代方案按代价排序：

1. **隐藏桌面 + 纯 L0 语义动作**（低成本，能力受限）。观察侧实测全部可用：窗口枚举、`PrintWindow` 截图都正常。适合"目标应用的关键动作都能用 UIA Pattern 表达"的场景。**建议先做一个 spike 验证 UIA 在隐藏桌面上的实际覆盖率**，再决定是否值得产品化。
2. **第二 Windows 会话**（高成本，能力完整）。独立会话有自己的输入桌面，合成输入可用。但需凭据认证、客户端 SKU 有并发限制。
3. **接受"前台执行"**（零成本）。明确告诉用户 Windows 上 agent 会占用桌面，配合"执行期间请勿操作"的提示。**对开发者个人机器上的工具，这可能是最诚实的选择。**

### P2：Linux 后台隔离

sway headless / Xvfb 会话管理。仓库已有 Xvfb 实践，增量不大。**[待验证]**

### P2：macOS 输入补全（前台模式）

CGEvent 补组合键与指针动作，先不碰后台。

### P3（需先决策）：macOS 私有 API 后台路径

在决定是否接受"依赖未文档化 API"这个长期成本之前，不要开工。建议先做一个 spike：在多个 macOS 版本上验证三个 SkyLight 符号的可用性和稳定性，再定。

### 明确不做

- **内核驱动级输入（Interception / ViGEm / 自研驱动）**。与项目定位冲突，反作弊冲突，部署成本极高。
- **PostMessage 作为通用后备**。失败方式是静默无反应，比明确报错糟糕得多。
- **绕过 UIPI**。把它做成清晰的边界和错误信息，而不是想办法翻墙。
- **`SwitchDesktop` 式的"后台"**。它会让用户当场黑屏，与目标背道而驰；只保留为调试用途。

---

## 6. 实测记录（Windows）

脚本都在 `tests/windows/`，可独立重跑。环境见文首。

### 6.1 输入通道可见性

**脚本**：`input_channel_probe.py`（被测端，按通道记录）+ `verify_input_channels.py`（驱动端）

探针注册 `RIDEV_INPUTSINK` 原始输入设备、hook 窗口消息、并用独立线程轮询 `GetAsyncKeyState`，三条通道分别记账。驱动端用不同编码发同一个键。

| 步骤 | SendInput | Raw Input | AsyncKeyState | 窗口消息 |
|---|---|:---:|:---:|:---:|
| `key_vk_A`（虚拟键） | 2/2 | ✅ 2 | ✅ 2 | ✅ 2 |
| `key_scancode_A`（扫描码） | 2/2 | ✅ 2 | ✅ 2 | ✅ 2 |
| `key_unicode_A`（**项目现用**） | 2/2 | ❌ **0** | ❌ **0** | ✅ 3 |
| `ctrl_a`（组合键） | 4/4 | ✅ 4 | ✅ 2 | ✅ 4 |
| `f1`（功能键） | 2/2 | ✅ 2 | ✅ 2 | ✅ 2 |
| `mouse_left_click` | 3/3 | ✅ 3 | — | ✅ 2 |
| `mouse_right_click` | 2/2 | ✅ 2 | — | — |
| `mouse_wheel` | 1/1 | ✅ 1 | — | ✅ 1 |

`key_unicode_A` 的窗口消息记录为 `vkey=231`（`VK_PACKET`）+ `WM_CHAR char=A`，印证了它是"投递字符"而非"模拟按键"。

**两个踩坑记录**（会影响别人复现）：

- `SetForegroundWindow` 首次返回 `ACCESS_DENIED`。调用进程不拥有前台时系统会拒绝，需 `AttachThreadInput` 到当前前台线程。第一次跑因此漏掉了整条窗口消息通道，**差点误判为"消息不工作"**。
- 焦点若设在 EDIT 子控件上，键盘消息进的是 EDIT 自己的 wndproc，探针（hook 在顶层窗口）收不到。必须把焦点留在被 hook 的窗口。

### 6.2 SendInput 与桌面的关系（关键否决）

**脚本**：`verify_hidden_desktop.py`（首次发现）→ `diagnose_hidden_desktop_input.py`（排除其他解释）→ `verify_switchdesktop_rule.py`（对照定论）

第一步在隐藏桌面上重现失败，并确认用户桌面未被打扰：

```
attached (SetThreadDesktop 成功):  sent=0/2  last_error=5 (ACCESS_DENIED)
mouse   (SetThreadDesktop 成功):  sent=0/3
user_desktop: foreground_unchanged=True  cursor_unchanged=True
```

第二步逐一排除其他成因：

| 假设 | 结果 |
|---|---|
| `GENERIC_ALL` 权限不对 → 改用显式 `DESKTOP_*` | 仍 `ACCESS_DENIED` |
| 句柄有问题 → `OpenDesktopW` 按名重开 | 仍 `ACCESS_DENIED` |
| 线程切换方式有问题 → 进程**出生**就在隐藏桌面（`initial_thread_desktop=AadDiagHidden`，全程不调 `SetThreadDesktop`） | 仍 `ACCESS_DENIED` |
| 对照：同进程在 `Default` 桌面 | **`sent=2`，成功** |
| `OpenInputDesktop()` 报告的输入桌面 | 全程 `Default` |

第三步只改一个变量做对照：

```
隐藏桌面非输入桌面:  sent=0/2  last_error=5   探针收到 0 条
SwitchDesktop 之后:  sent=2/2  last_error=0   探针收到完整链路
                     (raw_input vkey=66 + WM_KEYDOWN + WM_CHAR 'b' + WM_KEYUP)
input_desktop_now = "AadSwitchTest"（切换生效）
input_desktop_restored = "Default"（已还原）
```

**结论：`SendInput` 投递到输入桌面，与调用线程的桌面无关。** 而让隐藏桌面变成输入桌面就意味着用户黑屏，因此这条路无法用于"后台不抢输入"。

### 6.3 隐藏桌面上的渲染与截图

**脚本**：`verify_render_and_modifier.py`

同一个探针程序，分别在可见桌面和隐藏桌面各起一份，用相同的 16×12 网格采样 192 点比较：

| | PrintWindow | 尺寸 | 不同颜色数 | 主要颜色 |
|---|---|---|---|---|
| 可见桌面 | true | 640×320 | 8 | `0xFFFFFF`×171, `0xF9F5EC`×10, `0xA29964`×1 |
| 隐藏桌面 | true | 640×320 | 7 | `0xFFFFFF`×171, `0xF3F3F3`×14, `0xA29964`×1 |

分布高度一致，控件边框色 `0xA29964` 两边都在。**隐藏桌面上窗口确实正常渲染，`PW_RENDERFULLCONTENT` 能取到真实内容。**

**一个自我纠正**：`verify_hidden_desktop.py` 里的初版采样只取 4 个点，全落在空白区，得出"只有纯白、distinct=1"的结论，我一度据此判断"窗口没渲染"。加密采样后否定了这个判断。**"返回 true"和"取到 4 个白点"都不足以支撑渲染结论**，这个教训值得记下来。

（另：GDI 句柄在 ctypes 下必须显式声明 `restype`，否则 64 位句柄回传时溢出，`DeleteDC` 抛 `OverflowError`。）

### 6.4 修饰键泄漏

**脚本**：`verify_render_and_modifier.py`

发送 Ctrl 按下、故意不发释放，模拟批次中途失败：

```
before            async=False
press (仅 down)   sent=1
while_held        async=True   keystate=True
after 2s          async=True   keystate=True      ← 没有自愈
self_healed       False
after 显式 release async=False                     ← 只能主动复位
```

**系统不会自动复位修饰键。** 一次失败的组合键批次会让 Ctrl 永久 latched，此后用户敲 C 就是复制、敲 W 就是关窗口——影响远超本次动作。这条印证了 §4.3 的设计要求：影子状态 + 异常时主动补发释放，缺一不可。

---

## 7. 仍待验证清单

Windows 侧 4 条已闭环（§6）。以下仍未实测，**不要当作结论使用**：

1. **UIA 在隐藏桌面上的实际覆盖率。** winforms-mcp 声称"UIA 全部操作正常"<sup>[35]</sup>，但这是替代方案 1 的地基，必须自己验。
   → 在隐藏桌面上跑一遍现有 UIA 动作集（`invoke` / `set_value` / `toggle` / 树枚举），统计哪些可用。

2. **GPU 加速应用在隐藏桌面上的渲染。** §6.3 用的是 GDI 绘制的简单窗口，Chrome / Electron 这类 DWM 合成 + GPU 加速的应用未必一样。
   → 在隐藏桌面上起 Chrome，比较截图。

3. **macOS 26 上 `CGEvent.postToPid` 的行为是否真的收紧了？** SkyLight 三个符号在当前系统版本上是否仍可解析、可用？
   → 决定 macOS 路线是否成立。需要 macOS 机器。

4. **KDE Plasma 6.5 的 `pointer-warp-v1` 是否已落地？** GNOME 的 RemoteDesktop portal 是否真的没有持久授权？
   → 决定 Wayland 上是否要把 uinput 提到更高优先级。需要 Linux 机器。

5. **本项目已知的低级输入钩子拒绝问题**（`DRIVER.INPUT_BLOCKED`），在补了扫描码/虚拟键路径后是否表现一致？
   → 本次实测环境未触发该问题（所有 `SendInput` 都成功），需要在装有相关钩子软件的机器上复现。

6. **提权、RDP、Server SKU 下的行为。** §6 全部结论都在"未提权、本地交互式 Session 1"下取得。
   → 至少要覆盖"提权后能否操作提权窗口"这一条。

---

## 8. 参考来源

1. [SendInput function (winuser.h) — Microsoft Learn](https://learn.microsoft.com/fil-ph/windows/win32/api/winuser/nf-winuser-sendinput)（UIPI 约束、事件不与其他输入交错）
2. [KEYBDINPUT structure — Microsoft Learn](https://learn.microsoft.com/zh-tw/windows/win32/api/winuser/ns-winuser-keybdinput)（`KEYEVENTF_UNICODE` 合成 VK_PACKET，wVk 必须为 0）
3. [About Keyboard Input — MicrosoftDocs/win32](https://github.com/MicrosoftDocs/win32/blob/docs/desktop-src/inputdev/about-keyboard-input.md)（虚拟键 vs 扫描码的适用场景）
4. [SendInput 函数（中文版）— Microsoft Learn](https://learn.microsoft.com/zh-cn/windows/win32/api/winuser/nf-winuser-sendinput)
5. [UIPI blocks UIA enumeration and input injection on elevated windows](https://github.com/NousResearch/hermes-agent/issues/49067)（Medium→High 时返回空元素树）
6. [UAC: Only elevate UIAccess applications installed in secure locations — Microsoft Learn](https://learn.microsoft.com/ja-jp/previous-versions/windows/it-pro/windows-10/security/threat-protection/security-policy-settings/user-account-control-only-elevate-uiaccess-applications-that-are-installed-in-secure-locations)
7. [UAC: Allow UIAccess applications to prompt for elevation — Microsoft Learn](https://learn.microsoft.com/ja-jp/windows/security/threat-protection/security-policy-settings/user-account-control-allow-uiaccess-applications-to-prompt-for-elevation-without-using-the-secure-desktop)
8. [PowerTrackpoint — uiAccess exe 被 MSIX 协议激活拒绝（0x300D）](https://github.com/jordanbenevento-bot/PowerTrackpoint)
9. [UIPI issues with UI and browser automation actions — Microsoft Learn](https://learn.microsoft.com/bs-latn-ba/troubleshoot/power-platform/power-automate/desktop-flows/ui-automation/uipi-issues)（锁屏、UAC、最小化 RDP、提权目标等成因）
10. [Windows 锁屏界面输入检测方案](https://github.com/SpacJoy/Winlogon_Input_Detection/blob/main/README.md)（Winlogon desktop 隔离）
11. [input.key/type blind to RawInput/DirectInput-polling targets](https://github.com/WilliamIsted/agent-remote-hands/issues/64)（KBDCLASS vs MOUCLASS 合流点差异）—— **本机实测不成立，见 §6.1**：虚拟键与扫描码对 Raw Input 均可见，只有 `KEYEVENTF_UNICODE` 不可见
12. [DirectX/Unity/Unreal 游戏不使用消息队列](https://ngmaster.tistory.com/entry/%EC%89%AC%ED%94%84%ED%8A%B8%ED%82%A4-%EB%A7%A4%ED%81%AC%EB%A1%9C-%EA%B2%8C%EC%9E%84%EC%9A%A9)
13. [UI Automation — winapp-cli（post-message 绕 UIPI；WinUI3/UWP 无子 HWND）](https://learn.microsoft.com/sk-sk/windows/apps/dev-tools/winapp-cli/ui-automation)
14. [SendMessage 发送 WM_KEYDOWN 不触发响应的原因](https://ask.csdn.net/questions/9478392)
15. [Windows.UI.Input.Preview.Injection Namespace — Microsoft Learn](https://learn.microsoft.com/en-us/uwp/api/windows.ui.input.preview.injection?view=winrt-22621)
16. [UI Automation — winapp-cli（RDP/VM 会话中合成触摸可能不达目标）](https://learn.microsoft.com/nl-be/windows/apps/dev-tools/winapp-cli/ui-automation)
17. [Interceptor 驱动级输入模拟解析](https://blog.csdn.net/gitblog_00708/article/details/158492042)
18. [interception-python — 反作弊在驱动加载时拒绝启动](https://pypi.org/project/interception-python/)
19. [Inside macOS window internals: how SkyLight enables multi-cursor background agents — Cua](https://github.com/trycua/cua/blob/main/blog/inside-macos-window-internals.md)（CGEventPost 移动光标、postToPid 被 Chrome 丢弃、SLEventPostToPid、SLPSPostEventRecordTo、`_AXObserverAddNotificationAndCheckRemote`、右键降级、Canvas 类应用破功）
20. [macOS 26: CGEvent.postToPid not reaching Safari WebKit content](https://github.com/achiya-automation/safari-mcp/issues/29) **[待验证]**
21. [Apple Developer Forums — PostEvent / ListenEvent / ScreenCapture 三项 TCC 权限](https://developer.apple.com/forums/thread/820594)
22. [Using Secure Event Input Fairly — Apple Technote TN2150](https://developer.apple.com/library/archive/technotes/tn2150/_index.html)
23. [Exploring the Fragmentation of Wayland, an xdotool adventure（2025-11）](https://www.semicomplete.com/blog/xdotool-and-exploring-wayland-fragmentation/)（GNOME/KDE 弹窗且无持久授权、uinput 需 root 且是键码非符号、各 compositor 需各自实现）
24. [wdotool — wlroots 与 libei 双后端](https://crates.io/crates/wdotool/0.1.3)
25. [wlroots-bridge — 所需协议扩展清单](https://github.com/patrickjaja/wlroots-bridge)
26. [wayland-automation — KDE 鼠标支持预计 6.5 via pointer-warp-v1](https://pypi.org/project/wayland-automation/0.2.6/) **[待验证]**
27. [cua issue #2105 — GNOME/Wayland 无输入后端，但窗口枚举与截图不受影响](https://github.com/trycua/cua/issues/2105)
28. [ydotool README — 需要 /dev/uinput 与自定义布局问题](https://github.com/ReimuNotMoe/ydotool/blob/master/README.md)
29. [ydotool 权限配置（udev 规则 / input 组）](https://blog.csdn.net/gitblog_01040/article/details/151947453)
30. [Voxtype Troubleshooting — ydotool 按 US 布局发送原始键码](https://github.com/sk7n4k3d/voxtype/blob/main/docs/TROUBLESHOOTING.md)
31. [wayclick — 单个 uinput 绝对轴指针无法表示非矩形多屏并集](https://github.com/nalcorso/wayclick)
32. [Indirect display driver overview — Microsoft Learn](https://learn.microsoft.com/en-ca/windows-hardware/drivers/display/indirect-display-driver-model-overview)
33. [Windows 窗口站与桌面层次（Desktop 决定哪些窗口接收输入）](https://wenku.csdn.net/column/ab08dh3j66ac)
34. [Windows 中的桌面管理（消息只能在同一 Desktop 内传递）](https://blog.csdn.net/dxl2005/article/details/5968934)
35. [winforms-mcp Headless Mode — CreateDesktop 隐藏桌面；UIA/PrintWindow 可用，send_keys 不可用](https://github.com/fnrhombus/winforms-mcp/wiki/Headless-Mode)
36. [Roblox-Automation sandbox.py — SetThreadDesktop 工作线程 + SendInput](https://github.com/0xfray/Roblox-Automation/blob/main/sandbox.py) —— **其"隐藏桌面可 SendInput"的说法在本机实测不成立，见 §6.2**
37. [SetThreadDesktop function — Microsoft Learn（线程有窗口或钩子时失败）](https://learn.microsoft.com/en-gb/windows/win32/api/winuser/nf-winuser-setthreaddesktop)
38. [UiPath Picture in Picture — Child Session 与 Virtual Desktop 两种实现](https://docs.uipath.com/assistant/standalone/2023.10/user-guide/picture-in-picture)
39. [UiPath PiP - Virtual Desktop](https://docs.uipath.com/assistant/standalone/2023.10/user-guide/pip-virtual-desktop)
40. [UiPath PiP - Child Session（需 Windows 凭据认证新会话）](https://docs.uipath.com/assistant/standalone/2025.10/user-guide/pip-child-session)
41. [Windows 客户端版不支持真正的多用户并发 RDP](https://cloud.tencent.com/developer/article/2656201)
42. [tscon /dest:console 保持桌面活跃](https://www.cnblogs.com/baihuitestsoftware/p/15102165.html)
43. [RDP 断开后 RPA 失效的处理](https://blog.csdn.net/icestone2000/article/details/163159135)
44. [RemoteDesktop_SuppressWhenMinimized 注册表项](https://blog.csdn.net/cz1098564315/article/details/86712128)
45. [Session 0 隔离导致 Helper 无法驱动用户桌面](https://github.com/MinaH153/SuavoAgent/issues/62)
46. [native-devtools-mcp issue #5 — 同一 login session 内无公开 API 可做焦点保持的按键投递](https://github.com/sh3ll3x3c/native-devtools-mcp/issues/5)
47. [Cua's Background macOS Automation — launchctl 独立 login session](https://www.mech.app/articles/cua-s-background-macos-automation-how-gui-agents-need-session-isolation-without-stealing-focus/)
48. [Apple Developer Forums — launchctl asuser 在快速用户切换下的 bootstrap context 问题](https://developer.apple.com/forums/thread/692749)
49. [macOS 图形会话与显示设备绑定的限制](https://ask.csdn.net/questions/8917651) **[待验证]**
50. [Sway — swaymsg create_output 创建 HEADLESS 输出（ArchWiki）](https://wiki.archlinux.jp/index.php?title=Sway)
51. [mutter --wayland --headless --virtual-monitor 用法](https://github.com/eclipse-sirius/sirius-desktop/issues/432)
52. [Wayland Support Feasibility — headless compositor 对比与选型](https://github.com/quick-brown-foxxx/pyqt-agent-infra/blob/master/docs/backlog/wayland-support.md)
