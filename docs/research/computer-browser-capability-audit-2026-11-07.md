# Computer use / browser use 覆盖与限制审计

日期：2026-11-07。审查源码：`d50a6bde4fafb1a58cae5e611b2ff9ee0ad6e28a`，版本 0.0.6。
本机：Linux、KDE/X11。通过 PowerShell 7.4.6、osdk 管理的 Rust 构建当前 `aad` 后检查。

## 结论

普通 DOM 网页流程已具备较完整能力；桌面基础动作也确实可用，但**不足以承诺操作本机各类应用及全部系统功能**。
释放旧会话后，真实 Chromium 回归 14/14 通过；本机 Qt 窗口完成点击、中文输入、跨控件拖拽、窗口管理和截图/OCR。
然而实际应用出现了 Dolphin / Electron 整树阻断、KWrite 已聚焦仍被拒、系统设置搜索能力缺失，以及原生粘贴失败。
能力数量已经不少，真正的缺口主要在“看到以后是否还有合法、可执行的下一步”，以及能力声明能否兑现为界面效果。

“先观察、二次校验、不接受裸坐标”值得保留。它不必然排斥视觉操作：可以在被观察、能重新确认的窗口或
surface 内使用归一化位置。现在阻断任务的是观察完整性被扩大为整应用授权条件、surface 能力判断过窄、
同元素手势无法表达、焦点与可见性判断依赖单一无障碍返回值、部分权限策略禁止正常授权场景，
以及输入租约没有区分独立浏览器。

本报告重点评估 AI 的 `control_*` / `aad control serve` 接口。人工 `share` / viewer 通道已具备独立
down/up、修饰键等能力，不能把 AI 接口缺失误写成底层完全没有输入能力。AI 接口也不能靠人工远控通道
绕过它的观察与授权条件。

## 本次实测与证据范围

先运行真实入口 `aad probe` / `aad apps`，发现 18 个 AT-SPI 应用资源；这不是 18 个已验收可操作应用。
初始工具沙箱内无法连接 X11/D-Bus；获工具审批放行后两者均 available，因此没有将沙箱限制归因于项目。
旧 debug/release 分别是 0.0.2/0.0.4，本报告核心测试使用重新构建的 0.0.6。

构建命令：`osdk exec -- cargo build --locked -p aad-cli`，通过。
测试二进制 SHA256：`5c86c482c3b62b20a3d74b568a14a8b60d2d5972e64d674038b6fae999695b7d`。

原生输入/截图还依赖独立 C++ helper，`cargo build` 不会自动重建它们。检查发现本机残留 helper 缺少
当前的 window-info / paste 能力，旧 Qt fixture 也未开启拖出文本；已从本次源码重新构建并显式指定 helper，
相关结论以重测为准，不把旧产物导致的能力缺失、拖拽无效果归为当前代码缺陷。
构建元数据保存在 `helpers/build-info.json` / `helpers/fixture-build-info.json`；以下是重测产物 SHA256：

- XTest helper：`6eef6719365deebb70af3ce1f6fd47e21a17654f44aec2edfc8a74722214f857`。
- Capture helper：`3fcaa7d8ae3f2ecf7fdf7880ea9b47dfcce342100ea7017a439832ca4aecfd8c`。
- Qt fixture：`4a2d57dd5967f61a66b3e0940aee0faf639d94623c9a167afe1303b25cf93eba`。

| 检查 | 真实结果 | 证明范围 |
| --- | --- | --- |
| Codex 原生观察，max_nodes=2000 | 首次 733 节点，truncated=true；另次 777；find/hover 都返回 `CONTROL.OBSERVATION_TRUNCATED` | 该应用当前观察无法用于操作；不是单纯推测 |
| VS Code 原生观察 | 1332 节点，truncated=true；find/hover 同样拒绝 | 常用 Electron 应用也触发阻断 |
| Dolphin 原生观察 | 488 节点，truncated=true；find/hover 同样拒绝 | 文件管理器当前状态无法经该观察操作 |
| 上述三个应用缩小观察范围 | `CONTROL.SCOPE_UNSUPPORTED` | Linux 原生接口没有可用的局部观察恢复路径 |
| Konsole 原生观察 | 三个资源分别 408/351/379 节点，前两份完整，第三份截断 | 同类应用不同窗口状态也会不同；完整观察不代表输入已验收 |
| corplink 原生观察 | 首次 10 节点、仅 application 暴露 focus；另次 96 节点 | 页面/加载状态影响暴露内容，不能从发现进程推导控件可操作 |
| Qt 标准控件：fraction click、fill、key、type_text | 点击后状态实际改变；中文 fill 回读一致；Ctrl+A 后 Backspace 确实清空；Unicode 键入回读一致 | 桌面基本输入可用；键入测试临时切到 keyboard-us，随后恢复原输入法 |
| Qt 两个文本框间拖拽 | 目标框实际出现 `Drag across fields`，截图已查看 | 跨控件 drag 可用；不覆盖同画布拖动或跨应用拖动 |
| Qt 原生 paste | 新 helper、新 fixture 上仍返回 `DRIVER.PASTE_NOT_CONSUMED`，目标框保持空白 | 此环境粘贴未成功；独立比较证实剪贴板原文本恢复 |
| Qt 窗口管理、截图/OCR | 最大化、最小化、还原、关闭均回读状态；最大化尺寸实际变化；截图和状态文字 OCR 成功 | 常规窗口功能可用，不等于所有系统功能可用 |
| KWrite 编辑与保存 | 初次后台激活未成功；再次单独启动，窗口 active=true、编辑区 focused=true，GrabFocus 仍返回 false | 已聚焦编辑区仍被焦点接受条件拒绝；未完成编辑保存，临时文件保持原内容 |
| KDE 系统设置 | invoke 进入“快捷键”、返回“快速设置”成功；surface fraction click 后搜索框聚焦 | 已操作真实系统应用；分类导航与视觉点击成立 |
| 系统设置搜索框 | 画面可见但 AT-SPI visible/showing=false，文本节点仅有 focus，fill 被拒绝 | 搜索文本输入能力不完整；不能据此断言整个设置应用不可操作 |
| 独立 headless Chromium，本地合成页面 | 中文 fill、trusted fraction click 成功；页面回读 `saved:AAD 你好:trusted=true`；截图已查看 | 浏览器真实输入和效果成立；不是原生桌面点击证据 |
| 浏览器合成 password 输入 | `CONTROL.ACTION_UNSUPPORTED`，not_applied | 密码输入被能力策略禁止，未使用真实密码 |
| 普通 Canvas 的 fraction click | `CONTROL.ACTION_UNSUPPORTED`，not_applied | 看得到、截得到不等于能点击；该 Canvas 无 click 能力 |
| 暴露 drag 的 surface，从 fx=.2 拖到 .8 | `CONTROL.INVALID_REQUEST: drag destination must be a different node` | 不同位置但相同节点被协议拒绝 |
| 网页 Ctrl+K | `CONTROL.ACTION_UNSUPPORTED` | 页面编辑器常用组合也可能被统一快捷键白名单阻断 |
| scoped 网页，观察范围外计时更新 | `CONTROL.STALE_TARGET` | 真实 Chromium 已复现局部观察受无关 DOM 变化影响 |
| 2100 行的本地网页 | 返回 2000 节点、truncated=true；点击前部已观察 Save 按钮被拒绝 | 初始大页、点击扩展后大页均复现 |

初次输入均因另一工作区的 `aad mcp --allow-input --browser-port …` 持有 `desktop-input` 租约而返回
`CONTROL.RESOURCE_BUSY`。用户明确授权后，只结束匹配身份的旧进程 PID 2587067，再串行实测；未删除或修改租约文件。
释放后桌面与浏览器输入均可执行。不过 browser backend 仍无条件使用同一把锁，独立 profile 的 headless
Chromium 被无关桌面会话阻断这一设计限制依然存在。

已有真实浏览器回归执行命令（AAD_TEST_CHROME 指向本机 Chrome for Testing）：

```powershell
osdk exec -- cargo test --locked -p aad-control --test browser_e2e -- --ignored --test-threads=1
```

释放租约后重跑结果：**14 passed / 0 failed / 1 filtered out，24.93 秒**。
覆盖 iframe/OOPIF、open shadow DOM、输入/拖拽、上传下载、tabs/dialogs、页面读取、设备模拟及录制回放等。
首次成功启动浏览器的回归 0/14 是租约冲突，不能作为这些功能的缺陷数；更早的沙箱尝试在 loopback bind
阶段被 EPERM 阻断，未进入浏览器。测试浏览器均使用独立 headless profile；
浏览器回归与原生 GUI 输入串行安排。

本地原始请求/响应、截图、诊断摘要与复现实验脚本保存在忽略目录
`artifacts/capability-audit-2026-11-07/`；原生树可能含用户界面内容，不纳入提交。
关键摘要：`native-current-summary.json`、`fixture-fresh-summary.json`、`real-apps-summary.json`、
`kwrite-foreground-summary.json`、`settings-finish-summary.json`、`browser-summary.json`；
浏览器回归日志为 `browser-e2e-after-release.log`。截图包括 `native-fresh-drag.png`、
`settings-before.png` / `settings-after.png`、`browser-baseline.png`。
早期旧 fixture 的 drag 无效果以 fresh 结果替代；KWrite 前台重测中的截图请求因实验脚本复用旧观察而 stale，
不计入产品缺陷。测试创建的窗口均已关闭，输入法与剪贴板文本已恢复，未修改系统设置值。
尚未验收真实编辑器保存、文件管理器新建/重命名、跨应用完整任务或 Windows/macOS 真机行为。

## 本机最重要的缺陷：局部不完整导致整应用不能操作

独立回读 AT-SPI，而非重复解释 aad 的 `truncated`，确认了两类原因：

- Codex / VS Code 的最大观察深度到达 32；边界分别有 5 / 8 个节点仍有子节点。树并非超过 2000 节点才会截断。
- Dolphin 的 488 个节点中有 12 个 `role=unknown`、路径为 `/org/a11y/atspi/null`；查询 ChildCount /
  GetChildren 返回 UnknownObject。第三个 Konsole 资源有 2 个相同的空引用；这两份树最大深度分别为 11 / 10。
  独立检查未发现其余节点的计数差异或查询错误。

实现路径见 [AT-SPI backend](../../crates/aad-atspi/src/linux.rs)：`children` 未过滤空对象引用，
`read_node` 在属性失败后仍生成 unknown 节点，`capture` 的 GetChildren 错误分支将整份 snapshot 标为截断。
达到深度限制且仍有孩子、子数量变化、节点预算不足也会设置同一标记，输出没有具体截断原因。

[统一会话](../../crates/aad-control/src/lib.rs) 的 act（约 1111 行）和 find（约 1593 行）均拒绝整份
truncated observation。原生 provider 内部也有同样检查，CLI 不是恢复途径。
[NativeBackend](../../crates/aad-control/src/native.rs) 没有实现 `observe_scope`；Linux 观察以应用为入口，
control API 不暴露 max_depth，也没有以已观察窗口/节点为根重新完整观察的接口。

不能简单“遇到错误继续点”：需要区分规范空引用、真实读取失败、深度预算与动态变化；建立可证明完整的局部
观察，保留节点身份与动作前复核。否则为了避免选错一个元素，实际造成整个正常应用不可用。

## 完整树也可能无法输入：焦点、可见性与粘贴

KWrite 首轮观察 617 节点且完整，编辑器节点明确提供 click/type/key/paste 等能力。系统设置位于前台时，
激活 KWrite 超时且 active=false，随后的点击、按键、键入均在 `Component.GrabFocus` 返回 false 时拒绝。
这可以解释为跨应用激活受阻，因此额外关闭设置、单独启动 KWrite 重测：fresh 观察中窗口 active=true，
编辑节点 focused/visible/showing/editable 均为 true；focus 动作仍因 GrabFocus=false 失败，随后回读仍 focused=true。

[AT-SPI 焦点与输入前置检查](../../crates/aad-atspi/src/linux.rs) 300–315、868–889 将 GrabFocus 接受
当作必要条件，没有“验证已经聚焦后继续”的分支。第二轮实测的是 focus；type/key 共用这一前置路径是代码依据，
不是第二轮逐项输入测试。前置失败发生在 XTest 派发之前，外层 `UNKNOWN_EFFECT` 不足以证明按键已经发送。
需要补充同进程前台窗口、真实焦点目标的交叉验证，并准确区分未派发与效果未知；不能简单忽略 false 后盲目输入。

系统设置则展示另一类不一致：截图中可见的 QML 搜索框，AT-SPI 却报告 visible/showing=false，
导致标准 fill/type/paste 不开放。但 `invoke` 可进入“快捷键”分类，且锚定已观察的 QQuick surface、
根据截图使用 fraction click 能使搜索框 focused=true。这证明现有受控视觉路径已经有价值；仍需补齐
容器命中、子控件身份、可见性和焦点之间的验证，才能稳定接上文本输入，而不是假设一条 accessibility 状态永远正确。

原生 paste 在当前 Qt fixture 上失败，目标文本为空，剪贴板原文本独立比较一致。现有错误
`PASTE_NOT_CONSUMED` 不能区分未收到请求、请求者校验失败、不支持格式或丢失 selection ownership 等原因，
尚不能认定是 KDE 剪贴板管理器或 PID 校验导致。应增加不记录剪贴板内容的诊断，并在实际 KDE 会话验证。
键入还实测遇到 Fcitx 双拼启用时的 `DRIVER.INPUT_METHOD_ACTIVE`；临时切换 keyboard-us 后 Unicode 键入成功并恢复。
这不是完整的中文 IME 兼容验收。

这些结果证明部分常见真实应用流程会被现有前置检查阻断，不代表相应应用的所有操作都不可用。

## 能力与失败场景

| 场景 | 当前能力与边界 | 判断 |
| --- | --- | --- |
| 标准按钮、菜单、文本框、滚动、窗口切换 | 三端都有实现；本机 Qt 基础输入/窗口管理通过，但 KWrite 焦点、原生 paste 有实际失败 | 条件性支持，能力枚举不能当成应用验收 |
| 文件管理器/编辑器的大型树 | 本机已出现上述整应用阻断 | 当前优先级最高的可用性缺陷 |
| 自绘 UI、Canvas/WebGL、远程桌面图像、游戏/CAD | 可能只有容器节点，甚至没有可操作节点；Linux 指针能力还要求 Focusable | 缺少稳定的视觉目标闭环；不是所有 Canvas 都必然失败 |
| 同一画布框选、地图平移、签名 | 同一 target 作为拖拽两端被统一会话禁止，即使 fractions 不同 | 三个平台 AI 统一入口均受影响 |
| 文件管理器拖文件到另一应用 | 两端必须同一 resource / observation；Windows driver 另明确禁止跨窗口 | 当前动作不能表达；可替代上传/复制路径只覆盖部分任务 |
| Ctrl+click 多选、Ctrl+drag 复制、长按、曲线/套索 | AI Action 不含 modifiers、持续按住、路径或独立 down/up；Key 会完整释放按键 | 复合手势不足，不是禁止裸坐标的必然后果 |
| 密码登录与授权凭据输入 | Browser / Linux / macOS 禁止 protected 元素写入；Windows 允许保密写入 | 平台策略不一致；已有登录态/人工输入只能覆盖部分流程 |
| Chromium 业务网页 | 已有 iframe/OOPIF、open shadow、select/check、标签页/弹窗、JS dialogs、上传、下载 | 真实浏览器回归 14/14 通过，普通 DOM 流程较完整 |
| 大型网页 | 上限 2000；截断后整页拒绝；scope 仅支持唯一 HTML id | 本地 2100 行页面已复现 |
| 动态 scoped 网页 | scope 实际监听整个 document，范围外 DOM 变化也使目标失效 | 本次真实 Chromium 已复现 STALE_TARGET |
| 网页编辑器快捷键 | 所有 Alt/Meta、许多 Ctrl/Shift 组合被拒；Ctrl+S/K 等不能直接发送 | 页面内部快捷键与浏览器保留键未充分区分 |
| 浏览器设置、扩展页面、file:// | navigate/open_tab 仅 HTTP(S)；Firefox/Safari 不在 CDP launcher 范围 | 需原生 UI/独立能力，不能以 DOM 支持代替整个浏览器支持 |
| UAC、锁屏、登录桌面、提权窗口 | Windows 明确拒绝安全桌面；Linux helper 校验同 EUID；macOS 有 TCC/Secure Input 条件 | 权限边界，不能许诺通过放宽定位即可解决 |
| Linux Wayland / GNOME X11 | XTest helper 硬限定 KDE + X11 | 不等于普遍 Linux 桌面支持 |

主要代码依据：

- [统一动作定义](../../crates/aad-control/src/model.rs) 155–262：复合手势缺口。
- [统一会话](../../crates/aad-control/src/lib.rs) 1139–1155：同观察、不同节点的 drag 条件。
- [Linux 节点能力](../../crates/aad-atspi/src/linux.rs) 466–495：指针也要求 Focusable，文本输入另限 entry/text + Editable。
- [Linux 写入与截图](../../crates/aad-atspi/src/lib.rs) 808、855、1065：protected 写入、同节点拖动、截图前置条件。
- [Browser 观察脚本](../../crates/aad-control/src/browser.js) 12、40–68、82–95：密码策略、动作启发式、scope 与遍历。
- [Browser backend](../../crates/aad-control/src/browser.rs) 1572–1576、2846 起：全局输入租约、快捷键白名单。
- [XTest helper](../../plugins/linux_atspi/x11_xtest_helper.cpp) 103–112、216 起：会话与进程所有权限制。

额外静态发现：macOS drag 先 `fresh_target(source)`，内部 capture 替换 current snapshot，再用原 snapshot
执行 `fresh_target(destination)`，会触发 stale 检查。见 [macOS driver](../../crates/aad-macos-ax/src/lib.rs)
527、545、762、809；Linux 已有两端共用新观察的处理。**[待验证]** 需 macOS 真机、辅助功能授权，复核修复前后的
真实跨元素拖动效果。本次没有将源码路径判断写成 macOS 实测结果。

截图/OCR也不自动补齐上述缺口：截图仍要合法目标，Linux 对可见性、保护子树、遮挡等有检查；OCR 输出是只读
文本/比例框，不自动变成可写目标。Browser provider 提供 capture，未提供自身 read_text。
closed shadow 不在 browser.js 遍历范围内；会话连接前已打开的 JS dialog 也不能按普通路径接管。

## 与现有方案比较

下表为 2026-11-07 查阅的一手文档能力，不是同环境 benchmark。模型服务、执行器与完整 RPA 产品层次不同，
不能据工具数量推导任务成功率。

| 方案 | 官方公开能力 | 对本项目的差异意义 |
| --- | --- | --- |
| [Anthropic Computer Use](https://platform.claude.com/docs/en/agents-and-tools/tool-use/computer-use-tool) | 截图/zoom、视觉坐标操作、多种点击、拖动、按住与鼠标按下释放 | 无语义树界面仍有视觉操作路径；官方也不保证所有应用可靠 |
| [Anthropic Browser Use](https://platform.claude.com/docs/en/agents-and-tools/tool-use/browser-use-tool) | 元素引用、网页树、截图与坐标，另有可选 JS/文件等能力 | 不应再仅将其归类为纯视觉桌面；采用语义与视觉组合 |
| [OpenAI Computer Use](https://developers.openai.com/api/docs/guides/tools-computer-use) | 截图驱动 computer tool，亦可由模型调用代码执行中的浏览器/桌面库 | 本项目处于执行器层，可保留安全约束，但必须提供足够观察和动作 |
| [Microsoft Playwright MCP](https://github.com/microsoft/playwright-mcp) | 默认结构化快照；可用 `--caps=vision` 增加视觉坐标、mouse down/up 等；支持 Chromium/Firefox/WebKit | 结构化优先并不意味着放弃视觉恢复；浏览器覆盖范围更广 |
| [Browser Use](https://docs.browser-use.com/open-source/customize/tools/available) | 索引元素、输入、上传、滚动、tabs、内容提取、evaluate；[可配置截图视觉](https://docs.browser-use.com/open-source/customize/agent/all-parameters) | 不开放任意 JS 是可解释取舍，但需要补足常见类型化动作；未据此宣称其默认开放任意坐标 |
| [Power Automate Desktop](https://learn.microsoft.com/en-us/power-automate/desktop-flows/ui-elements) | 多种 UIA/MSAA 选择器；[官方建议](https://learn.microsoft.com/en-us/troubleshoot/power-platform/power-automate/desktop-flows/ui-automation/element-picker-cant-see-elements)用图像/OCR等恢复无法识别的元素 | 成熟 RPA 也不依赖单一 accessibility 通路 |

系统权限不是竞品就能自动绕过：[Windows SendInput](https://learn.microsoft.com/en-us/windows/win32/api/winuser/nf-winuser-sendinput)
受 UIPI 约束；[UIA 安全模型](https://learn.microsoft.com/en-us/windows/win32/winauto/uiauto-securityoverview)对 UIAccess /
安全桌面有额外条件。Power Automate 的 [UIAccess 模式](https://learn.microsoft.com/en-us/power-automate/desktop-flows/how-to/enable-ui-access)
也是显式部署条件。因此“本机运行”不代表天然可以点所有系统界面。

## 建议优先级与验收

1. **P0：恢复大型应用的观察与操作。** 正确处理 AT-SPI null 引用；暴露截断原因；提供按已观察窗口/容器进行完整局部观察。
   在当前 Dolphin、VS Code、Codex 上验收观察→定位→非破坏性动作→实际回读；不能只扩大节点预算。
2. **P0：兑现普通控件的输入能力。** 处理 KWrite 已聚焦但 GrabFocus=false 的情况，保持对真实焦点的复核；
   补齐 QML 可见性不一致时的受控输入路径，定位并修复 KDE 原生 paste。验收真实编辑器输入→保存→独立文件回读，
   设置搜索→匹配分类，以及剪贴板恢复；不要用专用 fixture 的成功替代真实应用验收。
3. **P1：使输入占用可诊断、可释放，并隔离独立执行环境。** 保留共享前台的互斥，研究独立 headless browser instance
   的租约范围；提供占用原因与正常关闭/释放路径。验收旧会话存在时独立 headless 流程仍可运行，真实桌面仍不会被并发输入。
4. **P1：扩大受控视觉操作与手势覆盖。** 延续已经验证的 surface fraction click，补同元素拖动、modifier click/drag、
   有界长按和路径；保持窗口/元素身份、时效、命中复核与动作后观察。不要用裸坐标跳过观察。
5. **P1：分离保密读取和授权输入。** 设计明确授权、日志脱敏的 secret 输入；统一三端与 browser 策略。
   不以可读出密码作为可写入凭据的前提。普通页面快捷键也需要与浏览器级动作分别设计。
6. **P1：修正 scoped DOM 失效范围与 macOS drag。** 局部观察只因与身份/位置相关的变化而失效；保留必要的祖先/layout
   校验。macOS 修复需真实效果验收，不能以 Linux 测试替代。
7. **P2：扩大系统与应用覆盖，并校验配套产物版本。** 明确 Wayland/其他 Linux 桌面路线，补 closed shadow / 自绘 / 远程桌面恢复策略；
   Windows、macOS、混合 DPI、复杂 IME、系统权限弹窗分别建立真实验收矩阵。
   打包/启动时明确 helper 协议能力与版本，避免新 Rust binary 静默搭配旧 helper。

后续修复应优先用本次失败的真实应用重新验收：KWrite 输入并保存临时文件；Dolphin 临时目录新建/重命名/撤销；
系统设置搜索并进入结果；动态 scoped 网页在范围外更新期间仍可操作稳定目标。
每步分别记录派发结果与实际界面/文件状态。新增 Windows/macOS 验收前，不把本机 KDE/X11 的成功外推到其他系统。
