# Windows、macOS 与 Linux 人工智能桌面自动化调研

> 调研日期：2026-08-24。本文聚焦通用桌面应用，而不是只操作浏览器；“无 OCR”指不通过文字图像识别获取内容，但可使用系统 Accessibility 语义树。市场产品的支持状态变化较快，上线前应复核版本。

## 1. 执行摘要

三端可以共用一套 Agent、任务协议和安全策略，但不能共用同一个底层桌面驱动。可行的产品路线是：

```text
应用原生 API / 浏览器 DOM / AppleScript 等确定性接口
                         ↓
Windows UIA / macOS AXUIElement / Linux AT-SPI2 语义树
                         ↓
语义动作（invoke、set value、select、toggle、scroll）
                         ↓
聚焦控件 + 键盘快捷键
                         ↓
由语义节点 bounds 导出的坐标点击
                         ↓
截图 + 非 OCR 视觉/VLM 定位
                         ↓
OCR（真正的最终兜底，可选）
```

最重要的结论：

1. **虚拟键盘鼠标不是每一步都需要。** 标准控件通常能通过 UIA Pattern、AX Action/可写属性、AT-SPI Action/EditableText 直接完成操作，这种方式不移动用户光标，也不依赖控件当前像素位置。
2. **要做“通用桌面 Agent”，输入注入仍然必须作为基础设施存在。** 快捷键、IME、hover、拖拽、画布、缺少可写 value 的编辑器以及 accessibility 实现有缺陷的应用都需要它。
3. **无 OCR 不等于无视觉。** Accessibility 树能给出文本、角色、状态、结构和坐标；图标、Canvas/WebGL、游戏、视频、远程桌面图像、自绘控件仍需要截图/VLM，里面的文字若要读则最终需要 OCR 或模型的视觉文字能力。
4. **不能给整个操作系统一个可信“识别百分比”。** 覆盖率的分母取决于应用集合、页面状态、控件粒度和“只读取”还是“能执行”。应对目标应用集实际扫描并统计。
5. **三端成熟度大致为 Windows > macOS > Linux。** 差距主要不在模型，而在 provider 完整度、权限机制和 Linux 桌面/Wayland 的碎片化。
6. **产品定位应是“支持集合内高可靠，未知应用 best effort”**，而不是“任意软件 100% 自动化”。

## 2. 市场与开源项目版图

市场上所谓 AI 桌面自动化主要分为四条路线。它们并不是同一种产品。

### 2.1 视觉计算机操作模型与智能体

| 产品/项目 | 主要环境 | 怎么看 | 怎么做 | 判断 |
|---|---|---|---|---|
| Anthropic Computer Use | 由开发者提供的桌面，参考实现常见 Linux VM/容器 | 截图 | 返回鼠标、键盘、滚动等动作，由客户端执行 | 通用但像素驱动；不是完整桌面运行时，也不天然读取原生语义树 |
| OpenAI Computer Use | 开发者提供浏览器或虚拟桌面 | 截图 | click/type/keypress/scroll/drag 等动作循环 | 同样是模型 + tool loop，不是 OS 驱动；官方示例可用 Playwright 或 `xdotool` |
| Gemini Computer Use | 主要针对浏览器 | 截图 | 坐标动作 | 官方模型卡明确主要为浏览器优化，不应当作三端原生桌面的成熟方案 |
| [OpenAI Codex Computer Use](https://developers.openai.com/codex/app/computer-use) | Codex App 已核实 macOS、Windows；未找到 Linux 本机 Computer Use 的官方承诺 | 屏幕图像 | 经系统授权后点击、输入、导航 | 已落地的本机产品，但控制前台桌面；不要与 API 的 computer tool 混为一谈 |
| [Microsoft Copilot Studio Computer Use](https://learn.microsoft.com/en-us/microsoft-copilot-studio/computer-use) | Windows 托管计算机、网站和桌面应用 | 屏幕视觉 + 推理 | 虚拟鼠标和键盘 | 企业 public preview；不是 macOS/Linux 本机助手 |
| UI-TARS Desktop / Agent TARS | UI-TARS Desktop 明确提供 macOS、Windows 本机 operator；Agent TARS 偏浏览器/工具混合 | Desktop 走视觉 grounding；Browser 可 DOM、视觉或混合 | 鼠标键盘/浏览器工具 | 很好的 VLM GUI Agent 参考；官方 quick start 仍写单显示器限制，不能据此宣称 Linux 本机桌面已正式支持 |
| Agent S / S2 | 研究框架，适配 OSWorld、macOS、Windows 等环境 | 以截图为主，可结合 grounding/知识检索 | PyAutoGUI/环境动作 | 研究价值高，不等于企业级三端执行器 |
| Self-Operating Computer | Windows/macOS/Linux（Linux 依赖 X server） | 截图/VLM | PyAutoGUI 一类键鼠执行 | 简单直观，适合原型；稳定性和权限边界需自己解决 |

视觉路线的共同循环是 `截图 → 模型选动作 → 执行 → 新截图 → 验证`。优点是能碰到没有 API 的 UI；缺点是速度、token、坐标漂移、遮挡、DPI、多显示器和误点击。VLM 能“看懂文字”在技术实质上仍是视觉文字识别，不能算严格的“无 OCR 语义读取”。

### 2.2 辅助功能 / 操作系统深度集成智能体

| 产品/项目 | 平台 | 核心路线 | 判断 |
|---|---|---|---|
| Microsoft UFO² | Windows | UIA、Win32、WinCOM、应用专用 API，加视觉推理 | Windows 原生深度和可靠性很有参考价值；UFO³ 文档虽覆盖多设备，不代表三端底层完全同构 |
| Cua Driver | Windows/macOS/Linux | 窗口级截图 + accessibility tree + 原生输入 dispatch | 与本文建议最接近的开源基础设施方向；仍需按目标版本、发行版和安全需求做资格验证 |
| pywinauto / FlaUI | Windows | Win32 + UIA/UIA2/UIA3 | 非 AI，但很适合作为 Windows deterministic executor 或实现参考 |
| Appium Windows / Mac2 | Windows/macOS | WinAppDriver/UIA、XCTest | 偏测试自动化，跨端 API 外观统一但驱动分裂；没有成熟统一 Linux 桌面驱动 |
| dogtail / Accerciser | Linux | AT-SPI2 | 非 AI；适合 Linux 语义树的验证、测试和实现参考 |

### 2.3 企业 RPA

| 产品 | 平台现状 | 感知与执行 | 判断 |
|---|---|---|---|
| Microsoft Power Automate Desktop | Windows | UIA/MSAA selector、浏览器扩展、图像、键鼠 | Windows RPA 标杆，不是三端本机统一产品 |
| UiPath | Windows 成熟，macOS 原生 UI 自动化已 GA；Linux 常见“跨平台项目/容器 Robot”不等于 Linux 桌面 UI 自动化 | selector、AX selector、DOM、Computer Vision、OCR、输入 | 企业治理和 selector fallback 值得借鉴；必须区别“在 Linux 跑 Robot”与“控制 Linux 桌面应用” |
| AskUI | Windows/macOS/Linux | 视觉 element detection、截图、键鼠，面向任何 UI/VDI | 三端覆盖强，但主卖点是视觉，而非纯 accessibility-first |
| Sema4.ai / Robocorp RPA.Desktop | Windows/macOS/Linux | 截图、模板匹配、OCR、键鼠；Windows 另有结构化 selector | 是跨平台脚本式 RPA 库，不是自主 GUI Agent；三端结构化能力不对等 |
| Automation Anywhere 等 | Windows 企业桌面为主 | selector/recording/视觉/OCR/键鼠 | 企业平台成熟，但并非开放的三端原生语义层 |

### 2.4 执行库、视觉工具和沙箱

- `nut.js`、PyAutoGUI：三端键鼠、截图和图像匹配；是执行层，不负责可靠语义理解。
- SikuliX/OculiX：截图模板/OpenCV，加 OCR 和键鼠；适合遗留 UI 或 VDI，但易受主题、分辨率和布局影响。
- E2B Desktop、Bytebot：重点是隔离的 Linux 虚拟桌面/容器和流式观看，不是用户本机三端原生驱动。
- OpenAdapt：以演示录制、视觉工作流、校验与受控修复为主；官方对原生桌面成熟度也采用按任务/环境资格验证的保守描述。

因此，真正值得组合的不是“挑一个库全包”，而是：

```text
UFO/Cua 的 OS 深度思路
+ RPA 的 selector、重试、审计、人工确认
+ UI-TARS/Claude/OpenAI 等 VLM 的未知 UI 兜底
+ 浏览器 CDP/DOM 和应用业务 API 的确定性快车道
```

## 3. 三大系统：无 OCR 能看到什么

### 3.1 横向能力矩阵

| 能力 | Windows | macOS | Linux |
|---|---|---|---|
| 主语义 API | Microsoft UI Automation（补充 MSAA/IA2、Win32、Java Access Bridge） | AXUIElement（补充 Apple Events/AppleScript） | AT-SPI2 over D-Bus |
| 树与语义 | role/type、name、value、state、AutomationId、relations、bounds | role/subrole、title、description、value、state、identifier、relations、frame | role、name、description、state、relations、attributes、bounds |
| 文本 | Text/Text2、Value；可读 range/selection/geometry | value、selected text/range、参数化文本 range/geometry | Text、EditableText、Hypertext |
| 表格/列表 | Grid、Table、Selection、VirtualizedItem | rows/columns/selected children 等 AX 属性，质量依 provider | Table、Selection、Collection |
| 直接语义动作 | Invoke、Value、Toggle、Selection、ExpandCollapse、Scroll、Window 等 | press、pick、increment/decrement、confirm/cancel、raise、show menu；写可设置属性 | Action、EditableText、Selection、Value、Component scroll 等 |
| 事件 | focus/property/structure/window/text 等 UIA events | AXObserver notifications | Registry event listeners over AT-SPI |
| 只用语义树需截图权限？ | 否 | 否；截图才需 Screen Recording | 否；Wayland 截屏通常需 portal |
| 键鼠后备 | SendInput | CGEvent | X11 XTEST；Wayland RemoteDesktop portal + libei |
| 主要权限障碍 | UIPI、目标提权、Session 0、UAC/锁屏 secure desktop | TCC Accessibility；通用 AX 与 App Sandbox 不兼容；安全输入/保护内容 | accessibility bus 可用性；Wayland compositor/portal 授权和发行版差异 |

三者共同能识别的核心不是“屏幕上所有像素”，而是应用主动上报给屏幕阅读器的对象模型。通常包括按钮、菜单、文本框、复选框、标签页、树、列表、表格、对话框、窗口和正文文本。颜色、图标真实含义、图片内容、Canvas 内部物体、视频内容不会自然出现。

### 3.2 Windows

Windows 是三端里最适合做 accessibility-first 自动化的：

- UIA 提供 Raw/Control/Content tree、通用属性、事件与强类型 Control Patterns。
- 标准 Win32、WinForms、WPF、UWP/WinUI 通常覆盖较好；Chrome/Edge/Electron/Qt 也能映射，但版本和应用实现会影响细粒度。
- 遗留应用可补 MSAA；富文本/浏览器可补 IAccessible2；Swing/AWT 要单独接 Java Access Bridge。
- `Invoke`、`SetValue`、`Select`、`Toggle`、`Expand`、`ScrollIntoView` 等无需模拟鼠标。
- Text Pattern 主要负责读/导航/选择，遇到没有 Value Pattern 的复杂编辑器，写入常需要键盘。
- `SendInput` 只允许注入到相等或更低 integrity 的目标。普通 agent 操作管理员窗口会受 UIPI 阻止。Session 0、UAC secure desktop、登录/锁屏均不能当普通桌面处理。

官方入口：[UIA Overview](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-uiautomationoverview)、[Control Patterns](https://learn.microsoft.com/en-us/windows/apps/design/accessibility/control-patterns-and-interfaces)、[winapp UI Automation](https://learn.microsoft.com/en-us/windows/apps/dev-tools/winapp-cli/ui-automation)、[SendInput/UIPI](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)。

### 3.3 macOS

macOS 的 AXUIElement 是跨应用 consumer API：

- 可按 PID 取得应用根，遍历窗口和控件，读取 role/title/value/state/frame，并枚举每个节点支持的属性与动作。
- 可直接 `AXUIElementPerformAction`，也可在 `AXUIElementIsAttributeSettable` 后写 value、focus、selection、窗口位置大小。
- 标准 AppKit 与正确实现 Accessibility 的 SwiftUI 较好；复杂 WebView/Electron、虚拟化列表和自绘控件需要实测。
- 对有脚本字典的应用，AppleScript/ScriptingBridge 能直接操作“文档、邮件、标签页”等业务对象，往往比 GUI 更可靠，但它不是通用能力。
- 语义树只需 Accessibility 权限；截图/VLM 另需 Screen Recording。发送 CGEvent 有系统授权。通用跨应用 AX 产品通常需 Developer ID 站外分发，因为核心能力与 App Sandbox 不兼容。
- 密码、protected content、Secure Event Input、登录/锁屏必须当作边界。

官方入口：[AXUIElement](https://developer.apple.com/documentation/applicationservices/axuielement_h)、[PerformAction](https://developer.apple.com/documentation/applicationservices/1462091-axuielementperformaction)、[SetAttributeValue](https://developer.apple.com/documentation/applicationservices/1460434-axuielementsetattributevalue)、[AX trust](https://developer.apple.com/documentation/applicationservices/1459186-axisprocesstrustedwithoptions)、[Scripting Bridge](https://developer.apple.com/documentation/scriptingbridge)、[ScreenCaptureKit](https://developer.apple.com/documentation/screencapturekit/)。

### 3.4 Linux

Linux 的 AT-SPI2 同样能给出结构化树，并提供 Action、Component、Text、EditableText、Selection、Table、Value、Hypertext、Document 等接口：

- GTK 标准控件通常较好；Qt Widgets 较好，但 QML/自绘 Item 需要开发者补语义。
- Electron/Chromium 可把 DOM/ARIA 映射到 AT-SPI；Canvas/WebGL 和错误/缺失 ARIA 仍是盲区。
- Swing 依赖 `java-atk-wrapper` 和具体 JDK/发行版配置；JavaFX 不应先承诺。
- 语义 `Action.DoAction`、EditableText、Selection、Value 不依赖 X11/Wayland 输入注入。
- X11 可用 XTEST/xdotool 合成输入。原生 Wayland 客户端禁止普通应用随意向其他应用注入输入；通用路径是 XDG RemoteDesktop Portal 获取用户许可，再经 libei，且 compositor 必须支持。`xdotool` 在 Wayland 中通常只能碰 XWayland 应用。
- `/dev/uinput` 可创建内核虚拟设备，但需要高权限并绕过 portal 安全模型，只适合受管环境的显式 helper。

官方入口：[AT-SPI2/libatspi](https://docs.gtk.org/atspi2/)、[D-Bus interfaces](https://gnome.pages.gitlab.gnome.org/at-spi2-core/devel-docs/xml-interfaces.html)、[Action](https://gnome.pages.gitlab.gnome.org/at-spi2-core/devel-docs/doc-org.a11y.atspi.Action.html)、[RemoteDesktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop)、[libei](https://libinput.pages.freedesktop.org/libei/)、[uinput](https://docs.kernel.org/input/uinput.html)。

## 4. “能识别多少内容”的诚实答案

没有官方或行业通用的 OS 百分比。正确衡量方式至少要区分：

- **Element recall**：人工标注的可交互元素中，有多少进入语义树。
- **Semantic completeness**：进入树的节点有多少具备正确 name/role/state/value。
- **Action coverage**：目标任务需要的动作中，有多少可通过语义 action 完成。
- **Task success**：在限定步数内，最终状态正确的任务比例。
- **Wrong-target rate**：执行在错误元素上的比例；这个指标比“发现率”更重要。

在尚未针对目标应用实测前，可以使用以下**工程估算区间**做规划，但不能作为产品承诺：

| 应用类型 | Windows | macOS | Linux | 无 OCR 能做什么 |
|---|---:|---:|---:|---|
| 系统标准 UI、规范原生控件 | 90–99% | 85–98% | 80–95% | 菜单、表单、列表、对话框、设置、窗口管理通常完整 |
| accessibility 做得好的 Electron/Web/Qt | 75–95% | 70–90% | 60–90% | 文本/按钮/表单可用；复杂编辑器与虚拟列表波动 |
| Office/IDE/复杂文档编辑器 | 60–90% | 55–85% | 50–80% | 导航、菜单和部分正文可用；大文档、绘图、插件面板需专用适配 |
| 自绘/Canvas/WebGL/游戏/远程桌面画面 | 0–30% | 0–30% | 0–30% | 常只能看见一个大容器、窗口标题或少量外壳节点 |
| 登录、锁屏、UAC/凭据等安全桌面 | 约 0%（按设计拒绝） | 约 0% | 取决于 greeter/session，产品应拒绝 | 不应绕过 |

这些百分比是基于 API 特征给项目预算用的先验估计，不是来自一个统一 benchmark。真正数字必须通过 `Accessibility Insights`（Windows）、`Accessibility Inspector`（macOS）、`Accerciser`（Linux）扫描你准备支持的 15–30 个应用和典型状态。

还要注意：“文字可读率”通常高于“动作覆盖率”。一个节点可能有 name/value，却没有 press/set-value；也可能能执行动作，但标签缺失导致无法安全定位。

## 5. 是否需要虚拟化键盘鼠标

答案分三层：

### 不需要

- 调用按钮的 Invoke/Press/Action。
- 通过 Value/EditableText 写入。
- 通过 Selection/Toggle/ExpandCollapse/Scroll 操作。
- 使用应用原生 API、AppleScript、Office COM、浏览器 CDP。

这些属于跨进程语义调用，可以在不少场景中不抢鼠标，甚至窗口在后台时也可工作；但每个 provider 是否真正支持要实测。

### 需要“合成输入”，但不需要虚拟 HID 设备

- 全局/应用快捷键、复杂编辑器输入、IME。
- hover、拖放、右键菜单、手势。
- 控件只暴露 bounds 而没有 action。
- 需要重现用户真实 hit-test 和焦点路径。

Windows `SendInput`、macOS `CGEvent`、X11 `XTEST` 已足够作为普通后备。它们会与真实桌面/焦点竞争，因此应检测用户介入并暂停。

### 可能需要虚拟设备或平台授权通道

- Wayland：用 RemoteDesktop portal + libei；这是 compositor 授权的“虚拟输入”通道。
- 受管 Linux 无人值守环境：可选 `/dev/uinput` helper，但风险和部署成本高。
- Windows 登录/UAC、macOS 登录/安全凭据界面：不应试图靠虚拟 HID 绕过；放到人工确认、系统 API 或专用受控环境。

所以 MVP 应实现语义动作，并保留标准 OS 合成输入；**不建议一开始自己写虚拟键盘鼠标驱动**。

## 6. 推荐的三端产品架构

```text
LLM / Workflow Planner（可云、可本地；不可信）
                 │ MCP / JSON SDK
                 ▼
Trusted Local Agent
  ├─ Session / Window Manager
  ├─ Snapshot + semantic tree normalizer
  ├─ Locator resolver
  ├─ Action loop + postcondition verifier
  ├─ Policy / confirmation / secrets / audit
  └─ Fallback controller
                 │ 私有本机 IPC
      ┌──────────┼──────────┐
      ▼          ▼          ▼
 Windows       macOS       Linux
 UIA/Win32     AX/Swift    AT-SPI/D-Bus
 SendInput     CGEvent     XTEST or Portal/libei
```

### 6.1 技术选择

- 公共核心：Rust，负责协议、resolver、动作状态机、安全与审计。
- Windows driver：`windows-rs` + UIA/Win32；以后再增加 IA2 和 Java Access Bridge。
- macOS helper：Swift/Objective-C，调用 AXUIElement/AXObserver/CGEvent；用稳定 bundle ID 和签名保持 TCC 身份。
- Linux driver：Rust + zbus/libatspi；MVP 锁定 Ubuntu 22.04/24.04 GNOME，明确测试 X11 与 Wayland。
- IPC：Protobuf；Unix domain socket / Windows named pipe。
- UI：Tauri 或独立薄壳均可，但不要让 Electron/Node native addon 成为 OS 驱动核心。
- 浏览器：单独增加 CDP/Playwright adapter，仍输出统一节点/动作 IR。
- Office 等高价值应用：增加 COM、Apple Events、UNO 或插件型适配器，优先于继续优化坐标点击。

### 6.2 统一节点与动作模型

`Node` 至少包含：

```text
node_id（只在 snapshot revision 内有效）
app identity + window identity
role / platform_role
name / description / value / help
enabled / focused / selected / checked / expanded / offscreen
bounds + display + coordinate transform
supported_actions
parent/children/labelled-by 等关系
backend provenance（uia / ax / atspi / dom / app-api）
```

公共动作：`invoke`、`focus`、`set_value`、`select`、`toggle`、`expand`、`collapse`、`scroll`、`key_chord`、`pointer`。

Locator 不要保存平台句柄或绝对 XPath。按以下顺序匹配：

1. 应用签名/bundle/executable + 窗口范围。
2. AutomationId/AXIdentifier/DOM id 等强身份。
3. role + accessible name + state + supported action。
4. labelled-by、ancestor、dialog、sibling 等关系。
5. 文本模糊匹配和历史 fingerprint。
6. hierarchy path 和 geometry。
7. `nth`、绝对坐标只能显式作为最后手段。

若第一、第二候选分差不足，应返回 `AMBIGUOUS`，不能偷偷点第一个。

### 6.3 每个动作都必须闭环验证

```text
observe → resolve → precondition → policy/confirm → execute
        → wait for event → re-observe → verify postcondition
```

发送、删除、购买、提交一类非幂等操作超时后必须返回 `UNKNOWN_EFFECT`，禁止自动重放。每次动作前重新解析 locator，避免窗口刷新后的 stale handle。

### 6.4 安全不是后续功能

- AI 规划器只提动作，本机可信代理最终判定风险和授权。
- UI 内文字一律是不可信 observation，防止 prompt injection。
- 应用 allowlist 使用签名/bundle/executable，不只看可伪造的窗口标题。
- 单桌面只允许一个 writer；检测物理鼠标键盘介入后暂停。
- 密码/token 用 `secret_ref` 到执行时本地解引用，不发给模型、不写日志。
- 发送、删除、付款、授权、安装等动作需要绑定目标和参数的确认 token。
- 截图默认本地、短 TTL；上传云端需单独授权。
- 驱动独立进程且每个 OS API 调用有 deadline，防止坏 provider 卡死整个 Agent。

## 7. MVP 与能达到的程度

### 推荐范围

4–6 人团队做一个可信三端 MVP，工程估算约 12–16 周：

1. **M0（1–2 周）**：统一 schema、三端树探针、权限探针、15 个目标应用矩阵。
2. **M1（3–4 周）**：先打透 Windows：observe/find/invoke/set/select/toggle/scroll/verify。
3. **M2（4–6 周，并行）**：接 macOS AX 和 Ubuntu GNOME AT-SPI，完成三端相同 fixture 流程。
4. **M3（3–4 周）**：输入后备、多显示器/DPI、非 OCR 视觉定位、安全确认、审计、崩溃隔离。

首版建议支持：标准表单、菜单、系统设置、文件选择器、文件管理、浏览器（加 DOM）、文本/表格基础操作、常见 Electron 应用，以及跨应用复制整理。

首版明确不承诺：任意游戏/Canvas/视频/VDI，安全桌面/登录/锁屏，任意 Linux compositor，无确认的付款/删除/发送，多个 Agent 同时控制同一个物理桌面。

### 现实目标

- 在经过资格验证的标准应用集：原子语义动作成功率目标可定 `>=98%`，5–10 步任务 `>=85%`。
- 在“任意未知应用”上：只能 best effort，不能给统一成功率。
- 纯视觉 benchmark 分数不等于企业可靠性；固定应用适配、业务 API、确定性 selector 和结果校验往往比换一个更大的模型更有效。
- 最终可做成“对 A 类应用接近 RPA 稳定性，对 B 类应用 AI 辅助恢复，对 C 类像素界面有条件降级”的产品，不是万能的人类替代。

## 8. 建议的下一步实验

不要先写完整 Agent。先做一个 2 周 capability probe：

1. 三端各实现 `list_windows`、`snapshot_tree`、`find`、`invoke`、`set_value`、`focus`。
2. 用官方 inspector 对 15 个应用建立 ground truth：系统设置/文件管理器/浏览器/VS Code/Slack 或飞书/Office 或 LibreOffice/终端/一个 Qt/一个 Java/一个 Canvas 应用。
3. 逐页面记录 element recall、semantic completeness、semantic action coverage、latency、crash/hang。
4. 再决定是否先投 Windows+macOS，Linux 暂定 Ubuntu GNOME，还是三端同时做。

这一步会把“能识别多少”从猜测变成你自己的、可复现的产品数据。

## 9. 主要参考资料

- Microsoft：[UI Automation Overview](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-uiautomationoverview)、[Control Patterns](https://learn.microsoft.com/en-us/windows/apps/design/accessibility/control-patterns-and-interfaces)、[Power Automate requirements](https://learn.microsoft.com/en-us/power-automate/desktop-flows/requirements)、[UFO²](https://github.com/microsoft/UFO/blob/main/documents/docs/ufo2/overview.md)
- Apple：[AXUIElement API](https://developer.apple.com/documentation/applicationservices/axuielement_h)、[NSAccessibility](https://developer.apple.com/documentation/appkit/nsaccessibilityprotocol)、[App Sandbox limitations](https://developer.apple.com/documentation/security/protecting-user-data-with-app-sandbox)、[Apple Events](https://developer.apple.com/library/archive/documentation/AppleScript/Conceptual/AppleEvents/intro_aepg/intro_aepg.html)
- Linux/Freedesktop：[AT-SPI2](https://docs.gtk.org/atspi2/)、[RemoteDesktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop)、[libei](https://libinput.pages.freedesktop.org/libei/)、[Wayland architecture](https://wayland.freedesktop.org/docs/html/ch03.html)
- Agent/RPA：[Anthropic Computer Use](https://platform.claude.com/docs/en/agents-and-tools/tool-use/computer-use-tool)、[OpenAI Computer Use](https://developers.openai.com/api/docs/guides/tools-computer-use/)、[UI-TARS Desktop](https://github.com/bytedance/UI-TARS-desktop)、[Cua](https://cua.ai/)、[UiPath UI Automation](https://www.uipath.com/platform/agentic-automation/rpa/ui-automation)、[AskUI Desktop](https://www.askui.com/solutions/desktop-testing)
- 衡量方法：[OSWorld](https://arxiv.org/abs/2404.07972)（其观测包含 screenshot 和可选 accessibility tree；任务成功率不能直接当作 OS 元素覆盖率）
