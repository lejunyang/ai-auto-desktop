# Computer use / browser use 能力补齐

依据：[2026-11-07 实测审计](../research/computer-browser-capability-audit-2026-11-07.md)。
目标是修复真实应用中已发现的阻断，并补齐审计列出的动作、观察恢复和平台覆盖；不能用 fixture 成功代替实际应用成功。
本文件记录完整待办与验收证据，未完成项不随某一轮提交结束而取消。

## 不变量

- 所有输入仍绑定已观察的节点并在派发前复核；键盘验证焦点，指针验证命中，拖拽验证两端。
- MCP 不提供裸屏幕坐标；视觉操作采用已观察 surface 内的比例位置。
- 真实桌面输入互斥；GUI 与浏览器效果测试串行调度，隔离浏览器只能按可信执行环境隔离租约。
- 系统权限边界不得通过绕过授权解决。UAC/安全桌面、TCC、Wayland 授权应明确支持条件及交接方式。
- 每项独立功能编译、相关测试与目标环境效果通过后立即本地提交。

## 实施与验收矩阵

| 项目 | 完成条件 | 状态与证据 |
| --- | --- | --- |
| 原生观察恢复 | 过滤规范 AT-SPI null 引用；输出具体截断原因；从已观察窗口/容器抓完整子树，动作复核也限定同一 scope；Dolphin/Electron 实测恢复合法操作 | null/截断诊断、局部观察已提交；Code/Codex 局部定位及刷新、Qt scoped click 通过。`e5f4f38` 以本应用焦点事件补充真实编辑框，Dolphin F2→中文重命名→磁盘回读通过；虚拟 table 父链不完整的 scope 仍拒绝。Electron 编辑/保存闭环待完成 |
| 焦点与 QML 可见性 | KWrite 已聚焦而 GrabFocus=false 时仍可安全验证并输入；设置搜索框在可见性元数据不一致时有受控输入路径；编辑保存与设置搜索实际回读 | 已提交 `f2282a5`；KWrite 中文编辑保存后磁盘回读一致，设置搜索“快捷键”截图与值回读一致，并打开匹配页面 |
| 同进程窗口焦点 | 键盘及带修饰键指针绑定 fresh 观察中的具体窗口；不能把另一同 PID 窗口当作目标，失焦及移动后释放自有输入 | `0af98e8`、测试 `b4d47a6` 已提交；私有 Xvfb 确认 PID-only 缺口并验证严格拒绝/释放，本机 Qt 双窗口真实事件计数及 Dolphin 中文重命名复测通过。第三方 Electron 编辑保存仍待验收 |
| 原生粘贴与 IME | KDE 原生 paste 实际输入成功并恢复原剪贴板；明确输入法条件，补充可工作的中文输入路径与诊断 | 已提交 `8421506`、`51a72ca`；隔离 X11 协议回归、KDE Qt/KWrite/QML 中文粘贴成功且恢复剪贴板。helper 无内容诊断与派发前错误已正确传递；复杂 IME 覆盖待补齐 |
| 浏览器局部观察 | 范围外无关 DOM 更新不误判 stale；范围内改动、身份替换、相关布局变化仍拒绝；大型页面可局部操作 | 已提交 `e8b8959`、`368177c`；Chromium 实测外部 ticker、相关 stale 拒绝、无 HTML id 的 scope_target、shadow root、scope token 刷新及 act.verify |
| 手势表达 | 同元素不同位置拖动、修饰键点击/拖动、有界按住和路径、跨资源拖拽具有完整端点复核与释放保证；真实效果验收 | 浏览器同 surface drag `d350c7b`、原生同元素 `9274fee`、三端与浏览器修饰键 `e46829b`、有界按住 `d314931` 已提交；Qt/Chromium 与 Dolphin 选择手势实测通过。路径接口及 Windows/macOS 实现 `4b10562` 已提交，目标 cfg 检查通过，真机效果待验；浏览器路径 `846b6ba` 已用 Canvas/closed-shadow 折线 PNG 与事件回读验收。Linux 路径与跨资源待完成；Linux 会话取消已通过 Qt 实际释放与 Xvfb 剪贴板恢复验证 |
| 视觉与自绘恢复 | Canvas/自绘/closed shadow 等缺乏语义控件时，通过可校验 surface 完成视觉操作；保留保护区域与命中限制 | Canvas trusted click/hover/scroll/drag 已提交 `d350c7b`，像素与可信事件回读通过；原生非 Focusable 指针 `31cf35a` 已在 Dolphin 验证，显式激活 `76cd6e1` 通过。closed shadow host 的视觉动作与保护 `3c7c2fc` 已通过 Chromium 27/27；内部语义节点仍不公开，其他原生自绘应用需逐项验收 |
| 保密输入 | 显式授权的 protected 输入与保密读取分离；三端/browser 策略一致，日志与录制不泄漏值，合成密码框验收 | `008974d` 已提交即时 secret_ref 授权路径；Qt 密码数字角色、替换/清空与独立状态回读通过，Chromium 21 项含敏感 DOM 身份恢复通过。普通输入不能绕过，持久化入口拒绝专用动作，子进程不继承绑定环境变量。Windows/macOS 实际效果 **[待真机验收]**；系统凭据库与保密工作流未实现 |
| 浏览器键盘与浏览器自身界面 | 页面编辑器快捷键与浏览器级动作分开；补明确授权的浏览器内部/本地页面操作路径，并明确非 Chromium 覆盖 | 页面快捷键隔离 `1a3bb77` 已提交；有界面 Chromium 18 组 Ctrl 组合只送页面、不误触浏览器 UI 的验收及异常 keyUp 通过。内部/本地页面授权 `527f5e4`、`ac4963b` 已提交，本地 HTML 点击、越界拒绝及 Chromium Settings 搜索输入回读通过。执行中祖先 URL/loader 复核与 history 授权 `d3f72eb` 已提交，3 项专项、完整 Chromium 34/34 和浏览器单测 21/21；非 Chromium 后端待实现 |
| 输入租约 | 保留共享前台互斥，独立 headless 不受无关桌面会话阻断；占用可诊断、可正常释放 | `2fe2395`、`562dcf5`、`6966e4c` 已提交。可信 headless 按 browser UUID 隔离，同实例仍互斥，未知/有界面实例仍占桌面；Chromium 19 项及保存工作流在桌面租约被占用时的实际执行通过 |
| macOS 拖拽 | 两端使用同一 fresh snapshot 复核；跨元素实际拖动验证 | 修复已提交 `8f92886`，25 项宿主逻辑测试及 clippy 通过；**[待验证]** macOS 真机 AX/TCC 与实际拖拽效果 |
| 跨资源拖拽准入 | 保留两个资源的有效观察，分别检查授权、范围与能力，获取两端输入租约；后端逐端 fresh 后执行 | `c343f69` 提供内部 opt-in/hook，保留每资源最新一份、每 provider 最多 16 份且受原 TTL 限制，输入尝试后整组失效。新增 9 项 fake 测试及 Session/views/native adapters 共 45/45 通过。当前生产后端均未启用，不等于已有跨应用拖拽；Linux 后端实现与真实效果仍待完成 |
| 系统覆盖 | GNOME X11 / Wayland 路线、Windows/macOS、DPI/IME/权限弹窗的实现与实际验收矩阵；不将 KDE 成功外推 | 用户确认仅有本机 Linux/KDE。`bea9187` 移除 X11 的 KDE 品牌限制，Xvfb 正反例及 KDE 新产物点击/截图通过；GNOME/XFCE 真机 **[待验收]**。Windows/macOS 实际效果 **[待真机验收]**。Wayland 输入后端 **[待实现、待真机验收]**，路线及证据见平台矩阵 |
| 配套产物一致性 | 发布/启动检测 helper 协议与能力，避免新 CLI 静默搭配旧 helper；构建和真实入口验证 | XTest 握手 `4d20ad6` 已提交；新 helper 正常提供动作，旧 helper 返回 `DRIVER.HELPER_INCOMPATIBLE` 并移除合成输入能力，语义 invoke 仍通过真实标签变化验证 |

## 当前协作分工

- 原生后端：null/截断诊断、scoped capture、焦点与可见性恢复。
- 浏览器：scoped DOM、独立执行环境租约及浏览器特有恢复路径。
- X11 helper：paste 根因、无内容诊断与修复。
- 主任务：统一 API / MCP / CLI 接线、跨后端手势与策略、串行真机验收、逐项提交和证据维护。

本机验收使用专用临时文件和测试窗口；保存失败必须报告失败，不能以动作返回成功代替文件内容回读。
原始 GUI 记录保存在忽略目录 `artifacts/capability-completion/`，提交中只保留脱敏证据、测试及实现。

## 验证记录与剩余失败

- `null-children-summary.json`：Dolphin 476 节点完整，三个 Konsole 资源完整；Code/Codex 的深度 32 截断仍如实报告。
- `native-scope/summary.json`：Code 1332 节点缩小为 5 节点、Codex 928 缩小为 6 节点；两者局部 find/token refresh 成功。该脚本因 hover 未通过而退出失败，不能记为全流程通过。
- `native-scope/effect-summary.json`：专用 Qt 按钮的局部观察→点击→独立状态回读通过。
- `focus/apps-summary.json`、`focus/settings-summary.json`：重新构建 CLI，分别使用专用 KWrite 文件和系统设置窗口；全选、中文粘贴、保存、搜索与分类导航通过。测试窗口已关闭，没有修改设置值。
- `pointer/summary.json`、`pointer/fixture-summary.json`：修复后 Dolphin 的 EWMH 显式 activate 达到 active=true；可聚焦按钮及不可聚焦容器 hover 均用真实指针位置回读验证，键盘焦点保持。
- `dolphin/gestures-summary.json`：Ctrl 多选、普通单选恢复、同 surface 框选通过；随后 F2 重命名未找到 AX 编辑框，因此脚本整体失败，不算重命名成功。`rename-missing-editor-main.png` 确认界面已经出现内联编辑框；仍需补观察路径。
- `helper-compatibility/summary.json`：同一 Qt 测试窗口比较旧新 XTest helper；旧版仅保留语义动作，invoke 后标签变化通过，未发送不支持的合成输入。
- `secret-reproducible/summary.json`：`tests/linux/verify-secret-input.ps1` 从源码构建 Qt 密码测试窗口，默认拒绝、普通 fill 拒绝、专用替换和清空的独立标签回读、值脱敏和密码框截图拒绝全部通过。Qt 5 的文字角色为 text、数字角色为 PasswordText，已修正为先检查数字角色。
- `browser-secret-targeted.log`、`browser-secret-full.log`：受保护输入实际替换、input/change 事件、默认拒绝、重新观察/重连后敏感身份保持、子 frame 元素删除/重挂/移除的截图保护及恢复通过；完整 Chromium 21/21。随后新增受保护节点不导入可能来自值的 AX name，针对性单测通过。
- 浏览器原始日志位于 `artifacts/capability-audit-2026-11-07/`；页面键另有 `browser-page-keys-headed.log`、`browser-page-key-timeout.log`、`browser-page-keys-full.log`。有界面用例用 AT-SPI 独立回读焦点和窗口集合，不只验证 DOM 事件。
- `dolphin/rename-component-probe.json`：重命名状态对已观察 cell 及其祖先，在文件图标/文字区域做只读 GetAccessibleAtPoint；仅返回现有 table 或 AT-SPI null，没有恢复出编辑控件，重命名仍未完成。
- `dolphin-focused-event/focused-event-summary.json`：仅订阅本次启动 Dolphin 的焦点事件，取得整树遗漏的真实 text/EditableText 控件；独立核对 PID、GetApplication、父链及尺寸通过。此时仅证明恢复路线，尚未算作重命名成功。
- `dolphin-focus-recovery/summary.json`：恢复实现后的正式 CLI 实测发现 focused/editable Text，补入的一个父链 context 保持只读；fill 中文文件名再 Enter，磁盘新名存在、旧名消失且内容一致。另一个 table scope 因虚拟父链断开安全拒绝，不能称所有 Dolphin scope 都可用。
- `window-focus-kde/summary.json`：Qt 双窗口故意保留主节点过期的 Focused 位；真实焦点在同 PID 副窗口时拒绝输入，两个窗口独立计数均为零，正确副窗口及激活后的主窗口各收到一次按下和释放。`window-focus-dolphin/summary.json`：严格窗口绑定后的中文重命名及磁盘回读仍通过。私有 Xvfb 协议测试另验证 type/key/paste/修饰键点击的同 PID 错窗拒绝、子窗口焦点允许、hold 途中失焦/移动的释放与严格几何匹配。
- `hold-kde/summary.json`：`verify-bounded-hold.ps1` 请求 200 ms，Qt 应用独立记录 F2 按下至释放 205 ms、按钮 201 ms、拖拽第一次移动 205 ms/释放 405 ms。此前可打印字母用例未收到计时状态，不能据此宣称 IME 长按已通过；改用 F2 隔离输入法影响。
- `hold-helpers/validation.json`：私有 Xvfb 独立回读键鼠状态与时序；长按、截止时间、helper 终止、焦点/遮挡变化后的释放，以及不接管外部已按住的键鼠通过。此批仅证明 helper 终止，正式 `control_cancel` 由下列后续用例验证。
- `native-cancel-kde/summary.json`：Qt 独立回报实际 down 后，经正式控制入口取消 5 秒 key/click/drag 长按，最新一次取消至 up 分别为 13/6/5 ms；后续普通输入成功。`verify-x11-cancel.ps1` 另验证长文本停止剩余字符、全局测试组合键释放、粘贴预读取消保留原 owner、已派发粘贴取消后独立读取原剪贴板一致。真实不读 stdin 的子进程与 4 MiB 测试载荷验证非阻塞取消；强杀缺少派发证据按 unknown。同步 D-Bus 调用仍只在调用前后检查取消，不能称立即中断系统调用。
- `browser-hold-targeted.log`、`browser-hold-full.log`：真实 Chromium 长按、拖拽起点停留、预算不足、焦点变化、遮挡和会话取消后的释放通过，功能提交前完整回归 24/24；日志同样位于 `artifacts/capability-audit-2026-11-07/`。
- `browser-closed-shadow-targeted.log`、`browser-closed-shadow-full.log`：closed host 默认 trusted click 后独立 PNG 像素回读、局部 drag/wheel、遮挡和 stale 拒绝通过；双密码字段 reveal 后移到 light DOM 仍脱敏并拒绝截图，移除后恢复。专项 2/2，完整 27/27，日志位于 `artifacts/capability-audit-2026-11-07/`。
- `browser-access-targeted.log`：服务器授权目录内本地 HTML 实际点击、未授权文件与内部来源拒绝、外部导航后重新检查通过；授权 `chrome://version` 可观察，`chrome://settings` 展开真实搜索 UI 后输入并独立遍历 DOM 回读通过。专项 2/2；祖先 frame 门禁单测、会话 19/19、CLI 6/6 及 control/CLI/MCP all-targets clippy 通过。`browser-access-capture.log` 与 `chromium-settings-search.png` 补验公开截图内存在输入值；截图仍在加载，未宣称搜索结果完成。上述文件位于 `artifacts/capability-completion/`。
- `browser-drag-paths-targeted.log`、`browser-drag-paths-full.log`：Canvas 与 closed-shadow 的折线由公开 PNG 独立回读，可信事件、按住/段间时序和遮挡/变化/超时/取消后的释放通过。专项 2/2，提交时完整 31/31；不支持 HTML5 自定义路径或跨资源路径。
- `browser-inflight-targeted.log`、`browser-inflight-full.log`：当前 URL 与祖先授权失效时，8 条键鼠/逐字符输入链停止并释放，同一 loader 的独立事件日志证明后续输入未继续；history 未授权文件拒绝、about:blank/data/长 HTTP 正例通过。专项 3/3、完整 34/34、单测 21/21 及 clippy 通过。本机 Chromium 跨文件路径 pushState 返回 SecurityError，fragment 更新正常；文件授权失效用移除自有临时文件模拟，不宣称已复现浏览器绕过。准备阶段取消使用确定性边界单测；异步 scroll 事件不能精确代表取消发生在某条 CDP 查询内。两组浏览器日志均位于 `artifacts/capability-audit-2026-11-07/`。
- `electron-native/summary.json`：独立 profile 的 VS Code，414 节点截断观察缩小到 31 节点完整编辑框；点击被 `DRIVER.POINT_NOT_PROVEN` 拒绝，focus/key/paste 返回已派发但实际文本不符，未继续保存，不算编辑成功。初始树含 Welcome 模态界面，随后 X11 检查发现同 PID 另一活动原生对话框；需要先处理启动界面，并用同进程双窗口 fixture 独立验证键盘焦点边界。仅凭动作之后的活动窗口不能断定此前每次输入落点。
- `electron-native-v2/summary.json`：严格窗口绑定后的复测，通过 Welcome 内已观察 Close 按钮的 focus→Enter 关闭活动窗口启动界面，再将编辑框缩小到 31 节点。Ctrl+A 与 paste 返回派发、剪贴板消费和恢复成功，但 AT-SPI 文本回读仍为原文加换行，未保存，仍不算编辑闭环通过。Welcome 的指针点击也仍被命中检查拒绝；需要独立核对 Electron 文本模型、焦点和选区。测试自有 profile 进程已关闭。
- Windows 修饰键代码已用 pin 工具链实际 `x86_64-pc-windows-gnu` cfg 做 check/clippy；这只证明编译，不能当成 Windows SendInput 效果验收。**[待验证]** Windows 交互桌面、相应权限与 DPI/IME 测试环境。

各功能的测试数字对应其提交时点；后续共享模型变化后仍需相应回归。未验证的平台与场景保持待办，不从 KDE/Chromium 的成功外推。
