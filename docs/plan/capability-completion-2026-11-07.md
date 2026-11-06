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
| 原生观察恢复 | 过滤规范 AT-SPI null 引用；输出具体截断原因；从已观察窗口/容器抓完整子树，动作复核也限定同一 scope；Dolphin/Electron 实测恢复合法操作 | null/截断诊断、局部观察已提交；Dolphin 整树完整，Code/Codex 局部定位及刷新通过；Qt scoped click 状态回读通过。Electron/Dolphin 指针实测尚未通过，继续处理激活和命中 |
| 焦点与 QML 可见性 | KWrite 已聚焦而 GrabFocus=false 时仍可安全验证并输入；设置搜索框在可见性元数据不一致时有受控输入路径；编辑保存与设置搜索实际回读 | 已提交 `f2282a5`；KWrite 中文编辑保存后磁盘回读一致，设置搜索“快捷键”截图与值回读一致，并打开匹配页面 |
| 原生粘贴与 IME | KDE 原生 paste 实际输入成功并恢复原剪贴板；明确输入法条件，补充可工作的中文输入路径与诊断 | 已提交 `8421506`；隔离 X11 协议回归、KDE Qt/KWrite/QML 中文粘贴成功且恢复剪贴板。helper 无内容诊断已加；Rust 错误传播及复杂 IME 覆盖继续补齐 |
| 浏览器局部观察 | 范围外无关 DOM 更新不误判 stale；范围内改动、身份替换、相关布局变化仍拒绝；大型页面可局部操作 | 已提交 `e8b8959`、`368177c`；Chromium 实测外部 ticker、相关 stale 拒绝、无 HTML id 的 scope_target、shadow root、scope token 刷新及 act.verify |
| 手势表达 | 同元素不同位置拖动、修饰键点击/拖动、有界按住和路径、跨资源拖拽具有完整端点复核与释放保证；真实效果验收 | 浏览器同 surface drag 已提交 `d350c7b` 并验证画布像素；原生同元素、修饰键实现中，按住/路径/跨资源待完成 |
| 视觉与自绘恢复 | Canvas/自绘/closed shadow 等缺乏语义控件时，通过可校验 surface 完成视觉操作；保留保护区域与命中限制 | Canvas trusted click/hover/scroll/drag 已提交 `d350c7b`，像素与可信事件回读通过，Chromium 17/17；原生非 Focusable 指针、closed shadow 等继续补齐 |
| 保密输入 | 显式授权的 protected 输入与保密读取分离；三端/browser 策略一致，日志与录制不泄漏值，合成密码框验收 | 待完成 |
| 浏览器键盘与浏览器自身界面 | 页面编辑器快捷键与浏览器级动作分开；补明确授权的浏览器内部/本地页面操作路径，并明确非 Chromium 覆盖 | 待完成 |
| 输入租约 | 保留共享前台互斥，独立 headless 不受无关桌面会话阻断；占用可诊断、可正常释放 | 占用 PID/会话/时间诊断已提交 `2fe2395`，锁竞争/释放/元数据 5 项通过；正常 close 原已释放，独立 headless 多锁隔离实现中 |
| macOS 拖拽 | 两端使用同一 fresh snapshot 复核；跨元素实际拖动验证 | 修复已提交 `8f92886`，25 项宿主逻辑测试及 clippy 通过；**[待验证]** macOS 真机 AX/TCC 与实际拖拽效果 |
| 系统覆盖 | GNOME X11 / Wayland 路线、Windows/macOS、DPI/IME/权限弹窗的实现与实际验收矩阵；不将 KDE 成功外推 | 待完成；其他目标环境需确认 |
| 配套产物一致性 | 发布/启动检测 helper 协议与能力，避免新 CLI 静默搭配旧 helper；构建和真实入口验证 | 待完成 |

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
- `pointer/summary.json`：Dolphin 的 EWMH activate 超时且 active=false；后续 hover 被 helper 拒绝，未观察到指针到达目标。需修复显式激活请求与错误诊断，再验证遮挡检查下的真实效果。
- Windows 修饰键代码已用 pin 工具链实际 `x86_64-pc-windows-gnu` cfg 做 check/clippy；这只证明编译，不能当成 Windows SendInput 效果验收。**[待验证]** Windows 交互桌面、相应权限与 DPI/IME 测试环境。

各功能的测试数字对应其提交时点；后续共享模型变化后仍需相应回归。未验证的平台与场景保持待办，不从 KDE/Chromium 的成功外推。
