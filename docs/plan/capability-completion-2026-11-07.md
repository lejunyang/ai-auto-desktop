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
| 原生观察恢复 | 过滤规范 AT-SPI null 引用；输出具体截断原因；从已观察窗口/容器抓完整子树，动作复核也限定同一 scope；Dolphin/Electron 实测恢复合法操作 | null/截断诊断、局部观察已提交；Dolphin 整树完整，Code/Codex 局部定位及刷新通过；Qt scoped click 状态回读通过。Dolphin 激活、指针、选择已通过；其 F2 内联编辑框仍未被 AX 暴露，Electron 指针待继续验收 |
| 焦点与 QML 可见性 | KWrite 已聚焦而 GrabFocus=false 时仍可安全验证并输入；设置搜索框在可见性元数据不一致时有受控输入路径；编辑保存与设置搜索实际回读 | 已提交 `f2282a5`；KWrite 中文编辑保存后磁盘回读一致，设置搜索“快捷键”截图与值回读一致，并打开匹配页面 |
| 原生粘贴与 IME | KDE 原生 paste 实际输入成功并恢复原剪贴板；明确输入法条件，补充可工作的中文输入路径与诊断 | 已提交 `8421506`、`51a72ca`；隔离 X11 协议回归、KDE Qt/KWrite/QML 中文粘贴成功且恢复剪贴板。helper 无内容诊断与派发前错误已正确传递；复杂 IME 覆盖待补齐 |
| 浏览器局部观察 | 范围外无关 DOM 更新不误判 stale；范围内改动、身份替换、相关布局变化仍拒绝；大型页面可局部操作 | 已提交 `e8b8959`、`368177c`；Chromium 实测外部 ticker、相关 stale 拒绝、无 HTML id 的 scope_target、shadow root、scope token 刷新及 act.verify |
| 手势表达 | 同元素不同位置拖动、修饰键点击/拖动、有界按住和路径、跨资源拖拽具有完整端点复核与释放保证；真实效果验收 | 浏览器同 surface drag `d350c7b`、原生同元素 `9274fee`、三端与浏览器修饰键 `e46829b` 已提交；Chromium 可信事件与像素、Dolphin Ctrl 多选和框选通过，随后普通点击恢复单选。按住/路径/跨资源待完成 |
| 视觉与自绘恢复 | Canvas/自绘/closed shadow 等缺乏语义控件时，通过可校验 surface 完成视觉操作；保留保护区域与命中限制 | Canvas trusted click/hover/scroll/drag 已提交 `d350c7b`，像素与可信事件回读通过；原生非 Focusable 指针 `31cf35a` 已在 Dolphin 验证，显式激活 `76cd6e1` 通过。closed shadow 等待完成 |
| 保密输入 | 显式授权的 protected 输入与保密读取分离；三端/browser 策略一致，日志与录制不泄漏值，合成密码框验收 | `008974d` 已提交即时 secret_ref 授权路径；Qt 密码数字角色、替换/清空与独立状态回读通过，Chromium 21 项含敏感 DOM 身份恢复通过。普通输入不能绕过，持久化入口拒绝专用动作，子进程不继承绑定环境变量。Windows/macOS 实际效果 **[待真机验收]**；系统凭据库与保密工作流未实现 |
| 浏览器键盘与浏览器自身界面 | 页面编辑器快捷键与浏览器级动作分开；补明确授权的浏览器内部/本地页面操作路径，并明确非 Chromium 覆盖 | 待完成 |
| 输入租约 | 保留共享前台互斥，独立 headless 不受无关桌面会话阻断；占用可诊断、可正常释放 | `2fe2395`、`562dcf5`、`6966e4c` 已提交。可信 headless 按 browser UUID 隔离，同实例仍互斥，未知/有界面实例仍占桌面；Chromium 19 项及保存工作流在桌面租约被占用时的实际执行通过 |
| macOS 拖拽 | 两端使用同一 fresh snapshot 复核；跨元素实际拖动验证 | 修复已提交 `8f92886`，25 项宿主逻辑测试及 clippy 通过；**[待验证]** macOS 真机 AX/TCC 与实际拖拽效果 |
| 系统覆盖 | GNOME X11 / Wayland 路线、Windows/macOS、DPI/IME/权限弹窗的实现与实际验收矩阵；不将 KDE 成功外推 | 用户确认仅有本机 Linux/KDE。其他平台实现与编译检查继续推进，Windows、macOS、GNOME/Wayland 的实际效果均 **[待真机验收]** |
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
- Windows 修饰键代码已用 pin 工具链实际 `x86_64-pc-windows-gnu` cfg 做 check/clippy；这只证明编译，不能当成 Windows SendInput 效果验收。**[待验证]** Windows 交互桌面、相应权限与 DPI/IME 测试环境。

各功能的测试数字对应其提交时点；后续共享模型变化后仍需相应回归。未验证的平台与场景保持待办，不从 KDE/Chromium 的成功外推。
