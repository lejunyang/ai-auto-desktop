# Linux KDE/X11 AT-SPI Rust 驱动

> 当前实现位于 `crates/aad-linux`。本机 KDE/X11 的历史验收证据截至 `1733e58c`。
> `window-geometry-proof-v1` 的验收环境和待验项见[平台能力验收矩阵](../testing/platform-capability-matrix.md)。

## 能力与实现

`desktop.linux_atspi` 由 CLI/MCP 直接注册，Rust 使用 `atspi`/`zbus` 连接当前用户 session
中的 accessibility bus。公开动作包括 `inspect_session`、`list_applications`、`snapshot`、
`find`、`focus`、`invoke`、`set_text`、`toggle`、`expand`、`collapse`，以及显式
`type_text`、`pointer_click`、`pointer_gesture`、`drag`、`key_chord`、`paste`、`capture_target`、
`capture`、窗口动作 `minimize`/`maximize`/`restore`/`close_window`/`activate`，和不锚定元素、
需单独授权的 `clipboard_read`、`global_key`。

生产代码不扫描 `/proc` 寻找其他用户的 bus，不猜测其他 session；环境、session bus、AT-SPI
registry 或平台 profile 不满足要求时返回 `DRIVER.UNAVAILABLE`。`inspect_session` 仍可返回
有界诊断，但不会把其他动作伪装成可用。

## snapshot、locator 与写前复核

应用按 PID/name/toolkit 等已声明字段精确选择；零匹配和多匹配都失败关闭。树抓取受
`max_depth`、`max_nodes` 和单次 children fan-out 硬上限约束。节点保留 AT-SPI bus/object path
identity、语义字段、状态、bounds、动作和 provenance；protected 值不读取。
Qt 5 的密码控件可能返回文字角色 `text` 而数字角色为 `PasswordText`；因此先读取数字角色
并据此标记保护，再决定是否读取文本。读取不到数字角色时不猜测它是普通字段。

`find` 只接受未截断快照。写动作必须同时携带 target 与原 locator，driver 以原预算 fresh
capture，重新唯一定位，并核对原生 identity、进程归属和语义 fingerprint。元素消失、变成
多义、被替换或无法证明一致时不会派发。`toggle`、`expand`、`collapse` 只在状态与 canonical
native action 同时满足受测映射时公开；Qt Widgets 按钮仅在唯一 `Press` 可用时公开 invoke。
控制会话可用已观察节点建立 scope，fresh capture 和动作复核保持同一范围；规范的 AT-SPI
null 子引用会被过滤，深度、节点预算和真实读取失败仍产生明确截断原因。

## Qt 坐标与窗口绑定

Qt AT-SPI 可能返回逻辑坐标，XTest 和根窗口截图需要物理像素。`597656c` 对每个窗口分别
校准，保留原始 AT-SPI bounds，并在观察中标记 `coordinate_space: screen_physical`。
匹配要求同用户的实际进程、完整应用顶层窗口清单、准确标题及双向唯一性。每次捕获用本次采集的
顶层 AT-SPI 和 X11 客户区几何建立候选清单。目标需要双向唯一的 client。其他顶层窗口的
候选即使彼此难以区分，只要都有可读的几何且都与目标的证明无关，目标就能绑定。
最小化窗口用其可证实的客户区几何参与排歧。合成输入坐标映射仅使用可见的 X11 client。
公开节点和操作授权仍限定在请求的 scope。只接受 X11 会话和有界、近似均匀的比例；元素边缘向内取整，
AT-SPI 命中查询将物理落点逆变换回相应窗口的逻辑坐标。

KDE 的 `_NET_WM_NAME` 可含应用后缀，故同时允许准确 `WM_NAME`。STRING 按 ISO-8859-1、
UTF8_STRING 按严格 UTF-8 解码；COMPOUND_TEXT 在有界 C++ helper 进程使用 Xlib 规范转换，
只有完整、无损、单字符串结果可用，且须匹配同一 XID 与原始属性字节。未知编码、截断和
歧义均不猜测。旧 helper 没有转换结果时，对需要该标题的窗口拒绝合成输入。

动作、截图和窗口操作还会复核原生对象、PID、XID 与两套几何；窗口移动或缩放变化后须
重新观察。不能证明坐标时保留语义观察及可用 native invoke/fill，移除合成输入和窗口动作，
拒绝按未知坐标截图。X11 读取使用单 worker 和截止时间，迟到结果丢弃，不累积阻塞线程；
缓存最多 32 个窗口、512 个 Qt bus 身份，超过身份上限需创建新会话。

`4c3e955` 的公开入口测试在本机单显示器上验证 Qt 1/1.5/2 倍应用缩放下的真实点击、F2
与按钮截图；同名歧义、窄 scope 不能消除歧义、非 X11 门禁也通过。后者仅是环境标签
拒绝测试，不是 Wayland 验收。8 项私有 Xvfb 标题检查通过；Dolphin 中文目录窗口里的
中文重命名，以及 KWrite 中文文件窗口的全选/粘贴/保存，均经独立磁盘回读确认。
混合 DPI 多显示器、旋转、热插拔与真实 Wayland **[待真机验收]**，非 Qt 缩放不能套用此结论。

## X11 helper 与 Artifact

AT-SPI 语义调用在 Rust 进程内完成。显式键鼠输入与截图使用固定路径的最小 C++ helper：

- `type_text`：聚焦并验证窗口后，以 XTEST 提交有界 UTF-8 文本；缺省 `unicode` 模式拒绝
  Fcitx5 组合输入。显式 `mode: "keys"` 则通过当前输入法发送已有布局按键；
- `pointer_click`：由 fresh bounds 计算中心点，经 AT-SPI subtree hit-test 与 X11 PID/focus
  复核后提交左键；
- `capture_target`：只截取 fresh 语义 target 的可见矩形，拒绝全屏、任意坐标、protected
  子树与遮挡，PNG 通过 run-scoped `ArtifactStore` 返回；`capture` 是同一检查与截图，
  直接返回 base64 PNG，供控制会话的 `control_capture` 使用，并把调用者的观察放回（读不消耗观察）。
  遮挡按堆叠顺序判断：窗口管理器 frame 里位于 client 之下的装饰/背景窗口不算遮挡，之上的才算。

helper 不调用 shell、`xdotool` 或 `uinput`；剪贴板只在 `paste` 与 `clipboard_read` 中经 X11
selection 协议访问（见下文）。观察前通过无输入的 `describe` 协商 XTest helper 协议和功能，
旧 helper 会显示 `DRIVER.HELPER_INCOMPATIBLE` 并移除不支持的合成动作，语义调用仍可用。
明确发生在派发前的拒绝保留 `not_applied`；首次输入/指针事件后的不确定失败按 `DRIVER.UNKNOWN_EFFECT` 处理；
成功仍须 fresh snapshot 验证。截图不会自动触发 OCR。

键盘输入额外要求 helper 声明 `window-focus` 和 `window-geometry-proof-v1`。Linux 观察按结构父链
记录顶层窗口。helper 将 AT-SPI 顶层范围与同 PID 的 X11 客户区和可信窗口管理器外框比较，
在两类候选的并集中确认唯一 client。初次比较要求宽高相等，原点各轴最多相差 2 px。外框必须
是 EWMH 窗口管理器实际拥有的客户窗口祖先，每边最多外扩 64 px。`_NET_FRAME_EXTENTS` 存在时
须与实际外扩一致。Qt 坐标校准继续使用客户区。

观察私有保存 client、实测客户区和外框链。键盘、粘贴、带修饰键的指针动作、同窗分段路径和
跨资源拖拽在派发时传回证明。helper 重读并逐字段核实，窗口移动、父框替换、外扩变化或候选
歧义都会拒绝旧证明。键盘焦点仍须位于 client 或其子窗口。指针命中仍按元素与 scope 检查，
同窗路径以客户窗口为界。持键或拖拽过程中继续复核，条件失效时释放本动作已经按下的键钮。
旧请求继续按客户区匹配。旧 helper 不提供依赖证明的合成动作，普通 AT-SPI 语义调用及原纯指针
入口仍可用。

显式 keys 模式额外要求 `type-text-keys`，只从当前 XKB group 和锁定状态中寻找现有键位与
Shift 组合，不创建 Unicode 映射、不使用剪贴板、不切换输入法；无法表示的字符在整个字符串
派发前拒绝。每笔按键复核窗口、PID、键位和状态，拒绝抢占外部持键，取消或变化时仅释放自己
按下的键。这个模式提交按键，最终文字由输入法决定；调用者可在 fresh 观察后发送锚定的
space/enter 选择候选，不能把 `submitted` 当成候选已确认。

本机 `verify-fcitx-physical-keys.ps1` 在新 Qt 窗口里使用已配置的微软双拼：`nihk` 产生可见
“你好”候选，space 后 Qt 的 `QInputMethodEvent` commit、字段值与 fresh AT-SPI 一致，
同时验证默认 Unicode 模式拒绝组合。截图只裁剪自己的前台测试窗口。测试显式选择并恢复
原方法，生产路径不会切换输入法；本机全拼虽已安装但未加入当前列表，未修改列表冒充覆盖。
私有 Xvfb 另验证已有键位、Caps、缺字拒绝、错误窗口、外部持键、取消与焦点/状态/键位变化。
其他方案、候选翻页和复杂组合 **[待真机验收]**。

非空 `drag.path` 还要求 `anchored-drag-path`，最多 32 个同一观察、同一元素内的归一化中间锚点；
起点和终点比例必须显式给出。按下前逐点检查，每段前完整 fresh AT-SPI 复核原生身份、
fingerprint、状态、原始 bounds 及前后点，释放前再查终点。窄 scope 另以最多 5000 节点、
128 层读取所属完整窗口，只用于遮挡校验，截断则拒绝；该私有读取不扩大授权成员或发布为
当前观察。其他可见分支覆盖锚点时保守拒绝，包括 disabled 覆盖层，因为 Qt 在鼠标抓取期间
可能继续把命中结果报告给原控件。未被 toolkit 暴露的非原生覆盖层仍不能保证被识别。

helper 每段先发带请求唯一 nonce 和顺序号的验证请求，收到对应 ACK 后才移动，并独立复核
X11 窗口/PID/几何/命中。Rust 只读 worker 不占主循环；AX 调用阻塞时取消和截止时间仍可
触发 helper 释放，迟到结果不再 ACK。每个 backend 同时只保留一个此类 worker，尚未退出的
只读请求会阻止新路径。

普通 X11 直线拖拽要求 helper 同时声明 `anchored-straight-drag` 和 `window-geometry-proof-v1`，
并携带起终点冻结的窗口几何证明。helper 在首次派发、按下、停留、每段移动和释放时复核两端的身份和几何，
按当前阶段核对鼠标落点的顶层命中。同应用双窗之间的中间段可以经过根窗口空白和两端已证明的 WM 外框，
按下和释放分别要求命中源和目标客户区。只检查源 X 客户端在按下后新建并映射的 override-redirect 临时窗。
可跳过的窗口须有空输入形状，或标为 `_NET_WM_WINDOW_TYPE_DND` 并有同源的活跃 `XdndSelection`。
helper 跳过这些图标后仍检查下方的真实遮挡。
修饰键额外要求源窗口焦点。派发后发现端点失效会停止移动，
释放本次按下的按钮，并返回 `DRIVER.POINT_NOT_PROVEN` 和 `unknown`。缺少能力时直线拖拽在输入前
返回 `DRIVER.HELPER_INCOMPATIBLE`，需同时升级 aad 和 helper。

在 X11 会话中，Linux provider 为每个已解析应用保留最新一份观察，合计最多 16 份，每份在
300 秒后过期。输入尝试使整组观察失效。使用旧 helper 的 X11 会话也适用这套保留规则。
Wayland 会话仅保留该 provider 的最新一份观察。所有入口共用可取消、有期限的调用门禁，
防止并行观察在输入期间重新发布有效目标。

X11 helper 同时声明 `anchored-drag-between` 和 `window-geometry-proof-v1` 时，Linux provider
额外启用双观察 `drag_between`。控制层继续使用 `drag`，分别保留起终点的
snapshot/revision/locator、scope 授权与输入租约。
双端 fresh 校验使用独立只读状态，仍通过 nonce/序号 ACK 与 helper 连接；源点 hover 后、
按下前，以及每段移动和释放前均复核两端身份、指纹、bounds、窗口与命中。
等待期间只读系统调用即使阻塞，helper 仍独立执行 deadline/cancel 与自有键鼠释放。

跨资源拖拽不接受非空 `path`；内部直线最大轴距离为 4096 屏幕像素，分 12 次移动。
每段逐像素检查所经窗口，仅允许两端 client、X 根、已核实同用户桌面和两端可信 WM frame。
与同应用直线拖拽共用 X11 命中检查，穿过源 X 客户端在本次按下后新建并映射的已核实拖拽图标，
继续检查图标下方的窗口。AT-SPI 复核两端窗口内的覆盖分支，顶层窗口的叠放顺序由 X11 检查。
frame 必须为端点的真实祖先、与 WM check 窗口属于同一 X 客户端，且每侧外扩不超过 64 像素；
未知装饰、外部应用和其余 override-redirect 覆盖均保守拒绝。down/up 只能发生在端点 client。
带修饰键时保持源窗口键盘焦点要求；不隐式切换到目标窗口。已有或等待期间新按下的外部键鼠不接管。

## 截图、OCR、窗口、粘贴、剪贴板与全局快捷键

与 Windows 同一组会话操作与授权（`--allow-input`、`--allow-clipboard-read`、
`--allow-global-keys`、`--allow-launch`），只在本平台可做到的范围提供：

| 能力 | 实现 | 拒绝 / 限制 |
| --- | --- | --- |
| `capture` | 上述 X11 root XGetImage，按 fresh bounds 裁剪 | protected 子树、被别的窗口或子树外可见节点覆盖、非 showing 均拒绝；最小化窗口不可截 |
| `read_text` | 会话对 capture 的 PNG 运行 `aad-ocr`（Tesseract，`prlimit` 资源限制）；BCP-47 映射为 traineddata（en→eng、zh-Hans→chi_sim 等） | 没有 tesseract 或 prlimit 时 `reads` 不列 `read_text`，调用返回 `DRIVER.OCR_UNAVAILABLE`；缺语言数据时同码并列出 `available_languages` |
| 窗口动作 | EWMH 窗口管理器按顶层观察中冻结的 client 与外框证明执行最小化、最大化、恢复、激活或关闭，最多 3 秒读回 `WM_STATE`、`_NET_WM_STATE`、`_NET_ACTIVE_WINDOW` 或窗口是否消失 | 最小化窗口在几何仍能唯一对应时可恢复。几何失配时隐藏窗口动作，证明失效时拒绝派发。状态没达到时返回 `DRIVER.UNKNOWN_EFFECT` |
| `paste` | 聚焦后 helper 先保存 CLIPBOARD 现有全部 target（单项不超过一次请求、合计 ≤32 MiB，否则 `DRIVER.CLIPBOARD_TOO_LARGE` 不粘贴），以真实时间戳占有 CLIPBOARD，只向拥有目标进程焦点窗口的同一 X client（`_NET_WM_PID`，或与该窗口同一 resource-id 基址——Qt/GTK 的取数窗口没有 pid 属性）提供文字，证明焦点后发 Ctrl+V；之后由一个脱离的 restorer 进程持有原内容直到别人占有剪贴板（最长 1 小时） | 目标始终没来取：`DRIVER.PASTE_NOT_CONSUMED`、effect unknown；protected 元素不提供；原先没有 owner 时恢复为无 owner |
| `clipboard_read` | 读 CLIPBOARD 的 UTF8_STRING（否则 STRING），有界 | 带 `x-kde-passwordManagerHint`（KDE / KeePassXC 的密码标记）时 `DRIVER.CLIPBOARD_PROTECTED`；INCR 大选区 `DRIVER.CLIPBOARD_TOO_LARGE` |
| `global_key` | XTest 组合键发给当前焦点，不锚定元素 | 拒绝 Ctrl+Alt+Backspace（X zap）、Ctrl+Alt+Delete、Ctrl+Alt+L、Super+L（`DRIVER.KEY_RESERVED`）；结果 `verified: false` |
| `launch` | 会话直接以 argv 启动白名单程序（`setsid`，不经 shell），按 AT-SPI 应用的 PID（或 `/proc/<pid>/exe` 的文件名）等待其出现 | shell、`xdg-open`、`gio` 等即使在白名单里也拒绝；不可执行文件拒绝 |

restorer 恢复的是内容的拷贝：原 owner 的延迟格式此时已被取成数据，这会被剪贴板管理器视为一次新的变更。
首次文字取数成功后，helper 继续响应同一已验证目标 client 的格式和元数据请求，直到
250 ms 请求静默或总交付窗口达到 2 秒；每批最多处理 32 个事件，使焦点、取消和截止时间
检查持续执行。这是有界协议策略，不保证任意延迟客户端都能完成取数。若期间出现新的
CLIPBOARD owner，保留其后续复制，不用旧内容覆盖；正常恢复和取消恢复仍须独立回读验证。

构建 helper：

```sh
plugins/linux_atspi/build_x11_xtest_helper.sh
plugins/linux_atspi/build_x11_capture_helper.sh
```

## 验证

对于 Dolphin 这类遗漏内联编辑框的无障碍树，后端在观察前只订阅当前应用唯一 D-Bus sender
的 focused 事件，不退为全桌面订阅。事件只提供对象引用；随后仍须核对连接 PID、GetApplication、
完整父链、真实 Text/Entry/PasswordText 角色、EditableText 接口、当前焦点及几何包含关系，
才能把编辑框补入观察。PasswordText 不读取 value；缺失的中间父节点仅作只读 context，
不能用于操作、find、截图或扩大 scope。切换应用和关闭后端时退出监听。
尚未开始观察前就已打开、且之后没有发焦点事件的编辑框不保证发现。
本机 Dolphin 已完成 F2→发现编辑框→中文重命名→磁盘回读；虚拟 table 的不完整父链仍拒绝 scope。

几何绑定改动前（证据截至 `1733e58c`），`verify-window-focus.ps1` 在本机运行两个同 PID Qt 窗口，
刻意模拟过期的 AX Focused 位与无效果的 focus 动作。错误窗口的操作被拒绝，两个实际事件计数均为零。正确聚焦或激活后，
对应窗口独立回报一次按下、一次释放。`verify-x11-window-focus.ps1` 在私有 Xvfb 复现旧
PID-only 判定的缺口，并验证新窗口绑定、子窗口焦点、长按途中同 PID 焦点切换和窗口移动后的释放。
这些测试不证明此前 VS Code 实验每一次输入的实际落点。后续独立 VS Code profile 已通过
`verify-code-paste.ps1`：所有输入走原生观察与操作入口，CDP 只读回选区、实际显示内容和截图，
磁盘读取确认中文替换及 Ctrl+S 保存。重复 focus 未破坏该案例的 Ctrl+A 选区；旧 helper 的
首次文字取数后过早恢复被私有协议回归复现，修复后真实 Code 不再在粘贴后挂起。
`verify-x11-paste-sequence.ps1` 另验证延迟三种文字格式、明确拒绝不支持格式、后续 owner
不被覆盖，以及连续请求仍在交付上限内结束。真实 Code 此次取数为 1 次文字和 18 次元数据，
不能把私有三格式案例当成 Code 的实际请求序列。Code 原生指针命中仍需单独解决。

`verify-x11-drag-path.ps1` 的私有 Xvfb 十项检查回读真实轨迹与键鼠释放；
`verify-native-drag-path.ps1` 的本机 Qt 七项检查来自几何绑定改动前（证据截至 `1733e58c`）。
窄 surface scope 外的 sibling overlay 也在覆盖范围内。实际事件确认正向路径三次移动、坏锚点零 down，
几何变化、遮挡或取消后收到 up。
小 fixture 的两中间点整窗口路径约 734 ms，窄 scope 加窗口复核约 1114 ms；取消后的 up
约 3 ms，窄 scope 遮挡后的 up 约 189 ms。这些是本次测试耗时，不能外推复杂应用或 32 点性能。

`verify-x11-cross-drag.ps1` 私有 Xvfb 17 项通过，独立双 PID 窗口回读 12 次持键移动，
验证可信/伪造 frame、单像素外部窗口、悬停覆盖、期限/取消、几何改变与外部持键保护。
`verify-native-cross-drag.ps1` 的本机 Qt 四项通过来自几何绑定改动前（证据截至 `1733e58c`）。
两个独立进程实际传递精确 MIME 数据并改变目标可见标签。取消后 45 ms、窗口变化后 77 ms 内返回并已释放，
悬停覆盖零 button-down。
成功动作约 4840 ms；两端每阶段 fresh 带来的成本不能用私有 Xvfb 协议耗时代替。

`window-geometry-proof-v1` 已在私有 Xvfb、D-Bus 和 bspwm 0.9.10 下复验。Qt 5.15.8 使用
无边框浮动窗口，按键、激活与两类拖拽的 helper 证明中 `frames` 为空。焦点拒绝与按放事件、
同窗逐段轨迹、双进程 MIME 与目标标签、1/1.5/2 倍点击和 F2 文本及 PNG 尺寸均通过。
缩放的歧义和非 X11 拒绝以及局部截图 scope 的覆盖与保护检查也通过。
这些 Qt 夹具禁用桌面平台主题，避免 Qt 在 XFCE 下加载 GTK3 主题并为同一进程注册一个没有控件的
GTK AT-SPI 应用。各脚本的命令见[可以复现的本机检查](../testing/platform-capability-matrix.md#可以复现的本机检查)。

私有 Xvfb、独立 D-Bus、GTK 3.24.38 和 xfwm4 4.18 实测中，带装饰和无装饰窗口的已观察
文本框点击均命中控件。布局键及默认 Unicode 各组 3/3，GTK 按键、逐字变化和 AT-SPI 最终值
一致。曲线拖放移开夹具图标热点后，带装饰和无装饰各 5/5，每次 12 段和一次实际放置。
默认 GTK 图标仍会遮住第三段的客户窗口命中。带装饰的 Qt、其他 WM 及文件管理器完整任务
**[待验证]**。

取消由每次调用显式传入的 `CancellationToken` 贯穿到 Linux backend。调用副本共享当前观察、
焦点订阅与录制状态，取消 token 不写回共享 backend；下一次调用不会继承取消状态。
helper 必须声明 `cooperative-cancel`。Rust 同时轮询子进程、token、截止时间与非阻塞 stdin，
即使 helper 不读取大输入也能取消。取消先发送 SIGTERM，再关闭未完成的输入管道；helper
停止后续字符/按键，释放自己按下的键鼠并恢复剪贴板，最多给清理 3 秒再强制结束。
已派发或被强制结束而缺少完整证据时返回 `unknown`，原观察失效。同步 AT-SPI D-Bus 请求
只在派发前后检查 token，不能承诺中断正在等待的 D-Bus 调用。

`verify-native-cancel.ps1` 在 Qt 应用独立报告实际 down 后通过正式控制入口取消 5 秒长按，
分别确认键、按钮、拖拽提前释放及下一次操作成功。`verify-x11-cancel.ps1` 在私有 Xvfb
确认长文本停止后续字符、全局测试组合键释放、粘贴预读取消不更换原 owner，以及粘贴派发后
取消仍能独立读回原剪贴板。没有用真实全局快捷键操作用户桌面。

CI 在私有 Xvfb、session bus 和 AT-SPI bus 中编译 Qt5 C++ fixture，并使用正式 Rust provider
验证应用枚举、snapshot/find、set_text、XTest Unicode 输入、中心点点击和目标截图：

```sh
tests/linux/run-rust-atspi-fixture.sh
```

截图、OCR、窗口、粘贴、剪贴板、全局快捷键与启动另有私有 Xvfb + D-Bus + openbox（EWMH 窗口管理器）
中的效果验证，每一项都以可观察结果判定（截图尺寸等于窗口 bounds、Tesseract 读出标签文字、粘贴后
字段值变化且 xclip 读回原剪贴板、全局 Ctrl+A/Backspace 清空焦点字段、最大化后应用自己报告的
宽度变大、最小化/还原经窗口管理器往返、启动的白名单程序作为 resource 出现、close_window 后进程退出）：

```sh
tests/linux/run-linux-parity.sh
```

历史隔离环境（WSL2 Ubuntu 22.04、Xvfb、openbox 3.6、Qt 5.15、tesseract 4.1）通过上述脚本。
2026-11-07 本机 KDE/KWin 真机另通过 Qt 窗口动作和截图、与 Klipper 共存的中文粘贴及恢复、
KWrite 编辑保存、系统设置搜索、Dolphin 选择手势、密码框替换和清空；范围及剩余失败见
[能力补齐计划](../plan/capability-completion-2026-11-07.md)。
`verify-x11-session.ps1` 在私有 Xvfb 中验证 KDE/GNOME/XFCE/无标签均能真实点击和截图，
同时验证 PID、焦点和 Wayland 拒绝；更换标签不等于验证相应桌面的窗口管理器。
**[待真机验收]** GNOME/XFCE 桌面、GTK3/4 paste、混合 DPI 多屏及复杂输入法场景。

fake backend 单元测试继续覆盖状态映射、toggle/expand/collapse、stale/truncated、deadline、
错误归一和 Artifact 边界。既有 KDE/KCalc 探索结果属于历史资格证据；当前自动门禁以仓库内
非 Python Qt fixture 为准。登录管理器、锁屏、其他用户会话、提权界面和 Wayland 输入均不
受支持。
