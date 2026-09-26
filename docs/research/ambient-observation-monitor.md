# 常驻桌面观察（Ambient Observation）调研与设计

状态：Draft，2026-09-26。本文回答一个具体问题：**能不能让 AI 长时间、低成本地观察本机在发生什么——用户切换到了哪个应用、应用内界面发生了哪些变化——并以"相对上一次的增量"而不是整树倾倒的方式输出。**

本文只记录经代码核实的现状与由现状推出的设计；凡未在真机验证的结论一律标注 `[待验证]`，并写明需要什么环境验证。录制回放（`docs/architecture/record-replay.md`）是相邻但不同的能力，本文不改动它的契约。

## 1. 使用场景与非目标

目标场景：一个常驻观察者持续记录"用户在哪个应用、应用里的什么元素发生了什么变化"，产出结构化、紧凑、可增量消费的事件流，供 AI 学习用户的工作方式并在事后/实时辅助。

由此推出的需求：

1. **常驻**：以小时计，不是固定十几秒的会话。
2. **应用切换可感知**：前台应用从 A 切到 B 时必须有一条明确事件，携带应用身份（进程、窗口标题、类名）。
3. **应用内变化可感知**：标签页切换、面板展开、对话框出现、文本值变化等。
4. **增量输出**：默认只描述相对上一次观察的差异（added / removed / changed），首次观察或请求时才给全量。
5. **文字与结构优先，视觉兜底**：以可访问性语义树为主；语义树看不到的区域（Canvas、部分自绘 UI、远程桌面）才显式截图 + OCR。
6. **平台**：Windows 与 macOS 都要支持（Linux 已有 AT-SPI 读写，事件面后续补齐）。

**非目标**：

- 不做全局键盘/鼠标钩子，不逐键记录（与录制规范 §5.5 同一条红线）。观察"值变成了什么"，不观察"按了哪些键"。
- 不做回放。本能力只读，产出物是观察日志，不是 workflow。
- 不做像素级视频录制。
- 不绕过任何既有权限边界（UIPI、TCC、secure desktop）。

## 2. 现状盘点（经代码核实，2026-09-26 main 分支）

### 2.1 一次性快照观察：三端具备，请求/响应式

| 能力 | 入口 | 实现位置 |
| --- | --- | --- |
| 枚举全部窗口 | `aad apps` / MCP `list_apps` | `aad-uia/src/windows.rs::list_windows`，以 `GetForegroundWindow()` 标记 `is_foreground` |
| 交互式元素描述 | `aad describe` / `describe_window` | 默认上限 80 节点 |
| 区域概览（不列内容） | `aad overview` | 带字符数上限与 `--out` 落盘 |
| 完整树快照 | `aad snapshot` | `Snapshot { snapshot_id, revision, nodes, digest, … }`（`aad-uia/src/model.rs:181`） |
| 单元素查找 | `aad find` / `find_element` | role/name/automation_id/class_name/near/inside/nth |

关键事实：

- driver 内部只保留一个 current 快照，`revision` 单调递增；它服务于 **stale handle 保护**（操作前二次校验，界面变了就拒绝），**不是为跨版本 diff 设计的**。
- `Snapshot::digest()` 已存在（`model.rs:197`），但只哈希 `node_id/role/name/automation_id`，**不含 value/class_name/states/bounds**，且全代码库没有树 diff 模块——"检测树变了"的半成品有，"说出变了什么"不存在。
- macOS `AxBackend` trait（`aad-macos-ax/src/lib.rs:67`）的方法集为 `list_apps / capture / same_element / focus / invoke / pointer_click / set_value / type_text`，其中 `same_element` 可用于跨次比较，但同样没有 diff 层。

### 2.2 事件捕获：仅 Windows，且是录制管线的一部分

实现：平台无关归并在 `aad-uia/src/capture.rs`，原生订阅在 `aad-uia/src/windows.rs::pump`（约 `windows.rs:1840`）。CLI 唯一入口是 `aad record <window> --seconds 15`。

订阅采用双机制（缺一种就瞎一半，实测见 `docs/plan/rust-port-status.md` §2.17–2.18）：

- **UIA 事件处理器**：`Invoke_Invoked`、`SelectionItem_ElementSelected`，以及 `Value` / `ToggleState` 两个 PropertyChanged。
- **WinEvent 钩子**：范围 `EVENT_OBJECT_FOCUS..EVENT_OBJECT_INVOKED`（0x8005–0x800C），补 WinForms / MSAA 老应用。

工程质量很高，且这些机制观察者应当直接继承：MTA 回调、有界 `EventBuffer`、溢出丢旧并计数、序号唯一且空洞有意义、会话数上限与 TTL、回调不向 COM 抛异常、盲区显式上报。

但它在四个根本点上不满足观察场景：

1. **锚定单窗口 hwnd**，不是桌面 root；进程退出订阅即销毁。
2. **固定时长**，无常驻模式。
3. **事件刻意不含 value**：属性回调的新值被显式丢弃（`record-replay.md` §11.4 的隐私决策）。观察场景恰恰需要值。
4. **focus 事件不产出记录**（只作上下文），而 focus/前台切换正是观察者最需要的信号之一。

另外，MCP 的 14 个工具全部是请求/响应式，**没有任何事件/订阅工具**。

### 2.3 应用内变化（如浏览器 tab 切换）

- `SelectionItem_ElementSelected` 已订阅：Chromium 标签栏在 UIA 树中是 TabItem，因此"捕获会话恰好锚定该浏览器窗口且存活"期间，tab 切换**预期可收到事件**，但无浏览器场景的真机验证记录。`[待验证]`
- 当前**未订阅** `StructureChangedEvent`（spike `spike_listen.rs` 里试过，产品代码 `pump` 没有），因此"对话框出现/面板插入/列表增删"这类结构变化在录制管线里不可见。
- 已知机制盲区（`record-replay.md` §3.2，真机对照实验）：点击不可聚焦元素、纯 hover/移动 → 0 事件；WinEvent 看 WebView 只到容器，DOM 内部无焦点/选中语义的内容更新不可见。

### 2.4 截图与 OCR

- `aad-ocr`（Tesseract）只接受调用方显式传入的图片，不能自行截屏、不会被隐式触发（roadmap「感知门」）。
- 截图：Linux X11 target capture 已验证；Windows 内容通道在 roadmap 中仍标"待真实 runner 证据"；无面向用户的 CLI/MCP 截图入口。
- 录制规范默认 `redaction.screenshots = none`。

### 2.5 macOS / Linux 事件面：不存在

- macOS：**没有 AXObserver 封装**，`AxBackend` 无任何 notification 方法；Swift helper 目前只服务读写。
- Linux AT-SPI：**没有 event listener 注册**（代码中只有输入验证用的事件计数）。
- 路线图（`record-replay.md` §9）原定事件录制分期为 Windows → Linux → macOS（macOS 受 TCC 约束最后）。

## 3. 需求差距总结

| 需求 | 现状 | 差距 |
| --- | --- | --- |
| 长时间常驻 | `record` 固定 15 秒、单窗口、进程退出即停 | 新建常驻观察会话及其生命周期管理 |
| 应用切换感知 | WinEvent 范围不含 `EVENT_SYSTEM_FOREGROUND`(0x0003)；订阅锚定单 hwnd | root/桌面级前台切换订阅 |
| 节点文字+结构描述 | 快照模型完整（role/name/value/states/actions/bounds/ancestry），已有分层与体积控制 | 可复用；面向 AI 的增量序列化需新写 |
| 增量描述 | 只有 revision 与不含 value 的 digest | **全新工作**：稳定节点身份 + 树 diff |
| 应用内变化 | 仅单窗口录制时部分可感知；StructureChanged 未订阅 | root 级订阅 + 结构事件 + 浏览器真机验证 |
| 截图/OCR 兜底 | OCR 可用但只吃显式图片；截图无产品入口 | 受策略控制的区域截图动作 |
| Windows | 读写完整，事件半成品 | 扩展订阅面 + 常驻化 + diff |
| macOS | 读写存在，事件面为零 | 新建 AXObserver 路径，工作量与 Windows 事件层相当 |

## 4. 关键设计决策

### 4.1 观察与录制是两条管线，不复用事件契约

录制器刻意丢弃 value、不记 focus、产物面向回放（locator + action）。观察者需要 value 与标题、以 focus/切换为一等信号、产物面向阅读。复用同一事件 schema 必然损害其中一方。

**决定**：新增独立的观察模块（工作名 `observe`），只复用录制管线踩平的**工程模式**（MTA、有界缓冲、丢事件计数、会话 TTL、回调不抛异常），不复用其事件类型与脱敏契约。两条管线共用底层快照能力，互不影响。

### 4.2 事件只负责"叫醒"，真相来自重拍快照

实测 UIA handler 对 WinForms/MSAA 全盲、WinEvent 看 WebView 只到容器（§2.2）。把事件本身当作"变化的内容"必然漏。

**决定**：采用 **事件触发 + 快照确认** 的混合模型：

```
原生事件（前台切换 / focus / structure / property）
   │  归一、去抖、按窗口聚合
   ▼
触发一次（或节流后的）前台窗口快照
   │  与上次该窗口的快照做树 diff
   ▼
输出增量 ChangeSet（结构/值/状态的增删改）
```

事件回答"**何时、大概在哪**该重新看一眼"，快照 diff 回答"**实际变了什么**"。这同时消化了双机制各瞎一半的问题：任一类事件到达都会触发一次完整语义重读，盲区只影响时延，不造成永久遗漏。

### 4.3 增量 diff：稳定身份是核心难点

node_id 只在单次快照内稳定，不能跨快照用。跨快照身份需要用"内容性身份"匹配：

**主身份键（候选，按平台可用性降级）**：

1. Windows：`automation_id` 非空时优先（同窗口内稳定，实测录制数据支撑其跨会话稳定率 100%，见 recording spec §7.2）。
2. 无 automation_id：`role + class_name + 稳定祖先路径 + 同名兄弟序号`。
3. 文本/叶子节点：退化为 `role + 父身份 + name/value 摘要`，允许"值变了但节点还在"的判定（即 value 不进身份键，进 diff 内容）。

**输出四类变化**：`added` / `removed` / `value_changed`（含 states 变化）/ `structure_changed`（子树增删或移动）。身份键与 value 必须分离，否则打字会被误报成"旧节点删除 + 新节点出现"。

匹配不上的节点对不强行配对：宁可报"一个 removed + 一个 added"，也不编造移动关系；后续可加基于位置的移动推断，但首版不做。

### 4.4 前台应用切换

- Windows：`EVENT_SYSTEM_FOREGROUND` 是系统级前台切换的权威信号，比 OBJECT_FOCUS 更准确（焦点可在同一应用内移动而不切换前台）。切换事件携带 hwnd → 复用现有窗口元数据解析（标题、pid、进程名、类名）。
- 切换时观察者动作：结束/挂起旧窗口的增量基线，对新前台窗口拍**基准快照**（可按配置只发概览或全量），之后进入该窗口的增量模式。每个窗口保留各自的上一次快照基线（用窗口身份键索引，而不是 hwnd——hwnd 会回收复用）。
- macOS 对应：`NSWorkspace.didActivateApplicationNotification`（应用级）+ AX `kAXFocusedWindowChangedNotification`（窗口级）。

### 4.5 轮询兜底，低频且自适应

对事件覆盖差的应用（WinForms、部分 Electron、远程桌面、提升权限进程），需要一条与事件无关的兜底：对当前前台窗口做低频快照 diff。默认空闲 1–2 Hz 量级、事件活跃时退避，具体数值真机标定。`[待验证：事件活跃窗口的自然事件率，决定轮询是否可长期关闭]`

项目里"静默丢弃与用户什么都没做无法区分"的教训同样适用：观察者必须周期性自证活着，并在输出里携带 `dropped_events` 与兜底轮询是否在起作用。

### 4.6 隐私：与录制器不同的取舍，但不放宽平台红线

- 观察者**保留**普通节点的 value 与窗口标题——这是场景目的（AI 学习工作内容），且 recording spec §5.1 的 2026-09-03 修订已确立"平台已划好界，普通值默认保留"。
- `states.protected`（密码框）红线不变：其值平台本就不提供，继续只报"有一个受保护字段"，不尝试任何绕过。
- 首版默认本地落结构化日志；是否上报、留存时长、按应用黑名单静音（如密码管理器、银行客户端）做成配置，不硬编码。
- 截图默认关闭，仅对"语义树声明为空/不可用但窗口确实在显示内容"的区域按需触发，并继承 OCR 的显式、最小区域、provenance 约束。

## 5. Windows 落地方案

### 5.1 模块划分（`aad-uia`）

- 新增 `src/observe.rs`：平台无关部分——观察事件类型、去抖/聚合、窗口基线表、树 diff（身份匹配 + ChangeSet 产出）、有界缓冲（复用 `EventBuffer` 的模式但事件结构独立）。
- `src/windows.rs`：新增 root 级订阅函数：
  - WinEvent：`EVENT_SYSTEM_FOREGROUND`、`EVENT_OBJECT_FOCUS`、`EVENT_OBJECT_NAMECHANGE`、`EVENT_OBJECT_VALUECHANGE`(若可用) 、`EVENT_OBJECT_INVOKED/SELECTION`，继续 `WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS`。
  - UIA（桌面 root element，`GetRootElement`）：`StructureChangedEventId`、`FocusChangedEventId`（需专用 `AddFocusChangedEventHandler`）、`SelectionItem_ElementSelected`、Value/Toggle PropertyChanged。
  - **root UIA subtree 全时订阅不可行（W1 已实测，见 §8）**：空闲桌面基线约 36 events/s，一次 30 秒会话 1972 个事件中 1760 个（89%）是后台进程的 structure 噪声（实测主要噪声源为 VS Code 的语言服务器状态条）。定稿：WinEvent 全局挂（成本低、天然带 hwnd）；UIA handler 挂 root 但**回调内第一时间按 `CurrentProcessId`（必要时加 `CurrentNativeWindowHandle`）与前台进程比对，非前台直接丢弃**，不进缓冲。相比"切应用时动态 remove/add 某 hwnd 的 subtree 订阅"，这样没有切换瞬间的订阅空窗；回调只做一次整数 pid 比较，开销可忽略，正式实现仍需长时 profiling 复核。
- 订阅线程模型沿用现有结论：root UIA handler 在 MTA 无需消息泵；WinEvent 回调投递到安装线程的队列，需要一个持续 pump 的专用线程（现有 `pump` 已证明此模式）。

### 5.2 生命周期与入口

- driver 侧新增 observe 类 action（全部只声明 `desktop.observe`）：`observe_start`（可带窗口过滤、节流参数）、`observe_poll`（取走聚合后的观察事件/ChangeSet，返回 dropped 计数）、`observe_stop`。与录制的 `capture_*` 平行、不共用会话。
- CLI 新增 `aad watch`：常驻进程，NDJSON/JSONL 每行一条观察记录（前台切换 / ChangeSet / 心跳），支持 `--window` 限定、`--full`（首帧全量）、`--poll-fallback-hz`、`--seconds`（0=不限）、`--out`。CLI 形态参考 `record` 的"进程持有订阅"事实，但不强制短时长。
- 长时运行的资源约束：单窗口基线快照占用上限、基线表 LRU 淘汰、会话 TTL 与 `capture` 一致。

### 5.3 ChangeSet JSON 形状（工作草案，实现时以 schema 为准）

```json
{
  "kind": "window_changes",
  "at": "2026-09-26T10:00:00Z",
  "window": {"window_id": "hwnd:…", "title": "…", "process_name": "chrome.exe", "pid": 1234},
  "baseline_revision": 7,
  "revision": 8,
  "trigger": ["structure", "focus"],
  "added":    [ {"role": "tabItem", "name": "新标签页", "identity": "…"} ],
  "removed":  [ {"role": "menuitem", "identity": "…"} ],
  "changed":  [ {"identity": "…", "role": "edit", "value": {"from": "草案", "to": "草案v2"},
                 "states": {"enabled": {"from": false, "to": true}}} ],
  "dropped_events": 0,
  "truncated": false
}
```

窗口切换是独立记录：

```json
{"kind": "foreground_changed", "at": "…",
 "from": {"process_name": "chrome.exe", "title": "…"},
 "to":   {"process_name": "Code.exe", "title": "…", "window_id": "hwnd:…", "pid": 5678}}
```

### 5.4 测试策略

- 平台无关部分（去抖、身份匹配、diff、LRU）纯 Rust 单测 + 变异测试，沿用仓库"注入缺陷须被测试捕获"的做法。
- 真机验证用现有 `tests/windows` 体系与 capture fixture：必须观察**可观察效果**（切换窗口、在 fixture 改值、开/关对话框），而不是命令返回成功。
- GUI 实测独占前台，不与其他真机测试并行（AGENTS.md 约定）。

## 6. macOS 方案（本期只设计，不实现）

- Swift helper 新增 AXObserver 注册：`kAXFocusedApplicationChangedNotification`、`kAXFocusedWindowChangedNotification`、`kAXFocusedUIElementChangedNotification`、`kAXValueChangedNotification`、`kAXSelectedChildrenChangedNotification`、`kAXTitleChangedNotification`；应用切换用 `NSWorkspace.shared.notificationCenter` 的 `didActivateApplicationNotification`。
- Rust adapter 的 `AxBackend` 增加 observer 能力（建议单独 trait，避免污染现有只读后端），仍通过现有 helper 协议回传。
- TCC：观察他应用需要 Accessibility 权限，屏幕内容另需 Screen Recording（仅截图路径需要）。helper 代码签名身份绑定意味着开发期每次重签可能重置授权——这是 roadmap 把 macOS 放最后的原因，真机验证需要固定签名身份的 Mac。`[待验证：各 AX 通知在 Safari/Chrome/Electron 上的实际投递率与 StructureChanged 等价物的粒度]`
- diff 层平台无关（§5.3），macOS 只提供快照与事件；身份键改用 AX `identifier`/`subrole`/祖先链（recording spec §1 已记录三端字段差异）。

## 7. 实施分期与验证清单

| 期 | 内容 | 退出门槛（真机可观察效果） |
| --- | --- | --- |
| W1 | ~root 级事件 spike：foreground 切换、root structure/focus/property 的事件率与噪声~ **已完成，证据见 §8**；下一阶段按 §5.1 的"前台 pid 过滤"方案落地 | ~~在真实桌面切应用、切浏览器 tab、开关对话框均收到事件；输出事件量统计；结论回写本文~~ 已完成 |
| W2 | ~Windows 观察会话：常驻 root 订阅 + foreground_changed + 有界缓冲/丢失计数 + `aad watch`~ 常驻会话与 `observe_start/poll/stop` 已完成并真机验证（§8.1）；**`aad watch` 入口与 trigger 聚合归 W3** | ~~连续运行 ≥10 分钟无订阅泄漏；切换 5 个应用全部产生且仅产生一条切换记录；dropped 可见~~ 20 秒 3 切换精确、0 丢弃、后台 pid 过滤为 0；10 分钟长稳在 W3 入口完成后补测 |
| W3 | ~树 diff 与增量 ChangeSet（含 value/states/结构增删），轮询兜底~ **已完成并真机验证（§8.2）：`aad watch` JSONL + trigger 聚合 + 树 diff + 1Hz 兜底 + 顶层窗口归属过滤** | ~~fixture 改值/开关控件/增删节点 diff 与人工核对一致；浏览器 tab 切换产出 added/removed；静默窗口零输出~~ 真实 Edge/Notepad/VS Code 端到端通过；单测覆盖 diff/身份/聚合；浏览器大变化降维留 W4 |
| W4 | 资源约束、按应用静音配置、截图/OCR 兜底接入 | 长时运行内存有界；密码字段不出现在任何输出；截图仅在语义缺口时触发 |
| M1 | macOS AXObserver + NSWorkspace，复用 diff 层 | 真机 Mac：应用/窗口切换、Safari/Chrome tab、文本值变化可观察；TCC 状态诚实上报 |

每期完成即按仓库 Conventional Commits 单独提交（实现/测试/文档分离）。

## 8. 实测证据（W1，2026-09-26）与待验证假设

探针：`crates/aad-uia/examples/spike_root_observe.rs`（`--features windows-native-examples`）。桌面 root 同时挂 UIA focus/automation/property handler 与全范围 WinEvent 钩子，有界采样 4000 条，只计数不注入任何输入。真机编排脚本在 30 秒内依次切换 VS Code → Edge →（两次 Ctrl+Tab）→ 记事本 → Windows Terminal → 设置 → Edge →（Ctrl+Tab）→ VS Code。

结果（1972 条，0 溢出）：

1. **前台切换权威可靠**：6 次编排的切换产生恰好 6 条 `EVENT_SYSTEM_FOREGROUND`，每条都带正确的进程名与窗口标题（如 `Code.exe / README.md - … - Visual Studio Code`、`msedge.exe / …`、`Notepad.exe`、`WindowsTerminal.exe`）。无重影、无丢失。
2. **root StructureChanged 是持续风暴，不能直接消费**：1760/1972（89%）为 `uia-structure`；**无任何交互时也有约 36 条/秒**，样本显示噪声源是后台 VS Code 的"Initializing Gradle Language Server"状态条反复结构变更。切换瞬间出现 364/秒、143/秒的峰值。
3. **浏览器 tab 切换可观察且语义干净**：两次 Edge Ctrl+Tab 各产生 1 条 `uia-selected`（control type 50019 TabItem），name 即目标页标题（实测两条分别为 Bing 搜索页与博客园页面）；同时伴随地址栏 `uia-value`（50004 Edit）变化。事件直接回答了"切到了哪个标签页"。
4. 其余通道：`obj-name` 112、`obj-focus` 21（含 Chromium "Chrome Legacy Window" 宿主噪声，需过滤）、`uia-focus` 19、`obj/state` 29、`obj/value` 10+7。WinEvent 与 UIA 双通道在真实应用上均有投递，印证"双通道都要挂"的既有结论在 root 级仍然成立。
5. 同线程（MTA + 消息泵）同时承载 UIA handler 与 WinEvent hook 可行，两类回调在 30 秒内均正常到达。

### 8.1 W2 产品会话真机证据（2026-09-26）

探针：`examples/observe_session_probe.rs`，走的是**产品 driver 契约** `observe_start → observe_poll → observe_stop`（不是裸 API），实现位于 `observe.rs`（缓冲/聚合/diff）、`observer_win.rs`（root 订阅 + 前台 pid 过滤）、`observe_session.rs`（会话生命周期）。

20 秒编排 Edge →（Ctrl+Tab）→ 记事本 → VS Code，结果：

- 契约闭合：`observe_start` 返回 `sources=["uia","winevent"]` 与 `observe_id`；`observe_poll` 每 500ms 取走事件并带 `dropped_events`；`observe_stop` 拆除后再 poll 明确返回 `DRIVER.OBSERVE_NOT_FOUND`。共 403 事件、**0 丢弃**。
- 前台切换 3 条（编排即 3 次），全部由 `winevent/foreground_changed` 精确给出，进程名与窗口标题正确。
- **前台 pid 过滤生效**：Edge（29:53–59）与记事本（30:00–03.8）前台期间，后台 VS Code 的 structure 事件为 **0**；切回 Code（30:03.869）后其事件才恢复。W1 的 36/s 后台风暴在产品层被挡在缓冲之外。
- 应用内信号到位：Edge 的 Ctrl+Tab 产出 2 条 `selection_changed`（TabItem）+ 2 条地址栏 `value_changed`；另有 `focus_changed` 15、`other` 16。

实测暴露、留给 W3 消化的一点：**前台应用自身的 structure 噪声仍很密**——切回 VS Code 后 6 秒内 225 条，全部来自其语言服务器状态条。这不是过滤问题（它确实是前台应用），而是"事件不能直接当内容"的又一证据：W3 的 `aad watch` 必须用 `observe::triggers` 把同一 (窗口, 类型) 的爆发聚成一次重拍，再由快照 diff 产出 ChangeSet，而不是逐条转发。

另：探针把 stdout 重定向到文件时中文窗口标题出现乱码，是重定向时代码页（非 UTF-8）所致的探针展示问题；`aad watch` 落 JSONL 时统一写 UTF-8，不复现。

### 8.2 W3 增量管线与 `aad watch` 真机证据（2026-09-26）

实现：`aad watch` CLI（JSONL），平台无关的 `observe::triggers`（按窗口+类型、静默窗口聚爆发）与 `observe::diff_trees`（跨快照身份匹配与 added/removed/changed），事件可经 serde 往返。循环严格采用 §4.2 的"事件叫醒、快照作证"：每批事件先聚合，再对**当前前台窗口**重拍一次快照、与该窗口上一次基线 diff，只输出增量；另有默认 1 Hz 的前台轮询兜底（可关）。

真机 22 秒编排 Edge →（Ctrl+Tab）→ 记事本 → VS Code（`--full`，落 UTF-8 JSONL 文件），最终干净输出 9 行：

- 3 条精确 `foreground_changed`；每个真实顶层窗口恰好一条 `window_baseline`（Code 368 节点、Edge 1000（触顶 truncated 如实标记）、Notepad 45）；切走 Edge 后其 tab 切换产出 `window_changes +670 -748 ~1`；全程 `dropped_events=0`、退出码 0。
- 迭代中实测并修掉两个归属陷阱，值得记录：
  1. **UIA 事件不能在回调里取窗口标题/句柄**。`CurrentNativeWindowHandle` 常指向内部渲染/弹窗（"Chrome Legacy Window"），对它拍快照只得到 1 个无用节点还会顶掉真窗口基线；而在 poll 时读前台标题又可能已切换。定稿：UIA 事件**只带 pid**，由消费侧按本批前台窗口（校验 pid 一致）归属。
  2. **必须拒绝把内部/无标题/最小化窗口当目标**。定稿：只对真实、有标题、可见的顶层窗口拍快照，其余事件跳过，避免噪声与基线驱逐。
- `--full` 基线含整树，单条 JSONL 较大（Edge 约 170KB），默认不带 `--full` 时首见窗口静默建立基线、只发增量，才是长期监控的形态。

待办（如实记录）：`window_changes` 在浏览器这类整树随滚动/动画变化的应用上 added/removed 量仍偏大（数百），W4 需要对"子树整体替换 vs 内容滚动"再降维（如区域级/可视区域 diff、按 fingerprint 配对 moved）；macOS 仍未开工。

由此把以下假设从"待验证"转为"已实测"：

- ~~[待验证] root 级 StructureChanged 是否风暴~~ → **是，空闲 ~36/s，需前台 pid 过滤**（方案见 §5.1）。
- ~~[待验证] Chromium tab 切换事件形态~~ → **`uia-selected`(TabItem, name=页标题)，可直接作为切换信号**；去抖窗口取 200–300ms 足够（两次 Ctrl+Tab 间隔 1.5s 均独立成条）。

仍待验证：

- [待验证] 仅靠事件、关闭周期轮询时，在 WinForms/Electron/远程桌面/提权应用上的漏报率（决定兜底轮询默认频率）。
- [待验证] 前台 pid 过滤方案在多窗口同进程（如多个 Edge 窗口、多个 VS Code 窗口）间是否需要进一步用 native window handle 收敛。
- [待验证] 自动化 ID 缺失时身份键（祖先链+兄弟序号）在动态列表（虚拟列表回收）下的误判率。
- [待验证] macOS AX 各通知在主流浏览器与 Electron 上的投递率（M1）。

## 9. 相关文档

- `docs/architecture/record-replay.md`：单窗口录制管线的机制、盲区与工程约束，观察者继承其工程模式但不继承事件契约。
- `docs/spec/recording-session-v1alpha1.md`：脱敏边界（§5）与三端能力不对称（§1）的实测依据。
- `docs/plan/phased-roadmap.md`：M3 中的录制回放分期与感知门（显式视觉、禁止隐式 fallback）。
