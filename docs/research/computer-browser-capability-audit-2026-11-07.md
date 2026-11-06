# Computer use / browser use 覆盖与限制审计

日期：2026-11-07。审查源码：`d50a6bde4fafb1a58cae5e611b2ff9ee0ad6e28a`，版本 0.0.6。
本机：Linux、KDE/X11。通过 PowerShell 7.4.6、osdk 管理的 Rust 构建当前 `aad` 后检查。

## 结论

当前适合无障碍信息完整的标准应用和普通 DOM 网页流程，**不足以承诺操作本机各类应用及全部系统功能**。
能力数量已经不少，真正的缺口主要在“看到以后是否还有合法、可执行的下一步”。

“先观察、二次校验、不接受裸坐标”值得保留。它不必然排斥视觉操作：可以在被观察、能重新确认的窗口或
surface 内使用归一化位置。现在阻断任务的是观察完整性被扩大为整应用授权条件、surface 能力判断过窄、
同元素手势无法表达、部分权限策略禁止正常授权场景，以及输入租约没有区分独立浏览器。

本报告重点评估 AI 的 `control_*` / `aad control serve` 接口。人工 `share` / viewer 通道已具备独立
down/up、修饰键等能力，不能把 AI 接口缺失误写成底层完全没有输入能力。AI 接口也不能靠人工远控通道
绕过它的观察与授权条件。

## 本次实测与证据范围

先运行真实入口 `aad probe` / `aad apps`，发现 18 个 AT-SPI 应用资源；这不是 18 个已验收可操作应用。
初始工具沙箱内无法连接 X11/D-Bus；获工具审批放行后两者均 available，因此没有将沙箱限制归因于项目。
旧 debug/release 分别是 0.0.2/0.0.4，本报告核心测试使用重新构建的 0.0.6。

构建命令：`osdk exec -- cargo build --locked -p aad-cli`，通过。
测试二进制 SHA256：`5c86c482c3b62b20a3d74b568a14a8b60d2d5972e64d674038b6fae999695b7d`。

| 检查 | 真实结果 | 证明范围 |
| --- | --- | --- |
| Codex 原生观察，max_nodes=2000 | 首次 733 节点，truncated=true；另次 777；find/hover 都返回 `CONTROL.OBSERVATION_TRUNCATED` | 该应用当前观察无法用于操作；不是单纯推测 |
| VS Code 原生观察 | 1332 节点，truncated=true；find/hover 同样拒绝 | 常用 Electron 应用也触发阻断 |
| Dolphin 原生观察 | 488 节点，truncated=true；find/hover 同样拒绝 | 文件管理器当前状态无法经该观察操作 |
| 上述三个应用缩小观察范围 | `CONTROL.SCOPE_UNSUPPORTED` | Linux 原生接口没有可用的局部观察恢复路径 |
| Konsole 原生观察 | 三个资源分别 408/351/379 节点，前两份完整，第三份截断 | 同类应用不同窗口状态也会不同；完整观察不代表输入已验收 |
| corplink 原生观察 | 首次 10 节点、仅 application 暴露 focus；另次 96 节点 | 页面/加载状态影响暴露内容，不能从发现进程推导控件可操作 |
| 独立 headless Chromium，本地合成页面 | 观察与 `control_capture` 成功，PNG 已独立查看 | 浏览器视觉观察链路工作；不是桌面点击成功证据 |
| 浏览器合成 password 输入 | `CONTROL.ACTION_UNSUPPORTED`，not_applied | 密码输入被能力策略禁止，未使用真实密码 |
| 普通 Canvas 的 fraction click | `CONTROL.ACTION_UNSUPPORTED`，not_applied | 看得到、截得到不等于能点击；该 Canvas 无 click 能力 |
| 暴露 drag 的 surface，从 fx=.2 拖到 .8 | `CONTROL.INVALID_REQUEST: drag destination must be a different node` | 不同位置但相同节点被协议拒绝 |
| 初始即有 2100 行的本地网页 | 返回 2000 节点、truncated=true；点击前部已观察 Save 按钮被拒绝 | 大页面限制已在真实 Chromium 复现 |
| `desktop_launch`、浏览器普通 fill/click | `CONTROL.RESOURCE_BUSY`，not_applied | 既有会话占用输入租约，本次没有取得成功输入的效果证据 |

最后一项有明确环境原因：已有另一工作区的 `aad mcp --allow-input --browser-port …` 进程持有
`desktop-input` 租约。已向用户询问是否允许关闭旧会话；未收到授权前不结束进程、不删除锁、不改租约目录。
浏览器 backend 也无条件使用同一把锁，所以独立 profile 的 headless Chromium 同样受阻。

已有真实浏览器回归执行命令（AAD_TEST_CHROME 指向本机 Chrome for Testing）：

```powershell
osdk exec -- cargo test --locked -p aad-control --test browser_e2e -- --ignored --test-threads=1
```

编译成功，解除工具沙箱限制后 **0 passed / 14 failed**，均在需要输入时受上述租约冲突阻断。
DOM/AX 读取成功。**这不是 14 项浏览器功能均有缺陷，也不是这些功能本次验证通过。**
测试浏览器均使用独立 headless profile，没有并行抢占桌面前台。

本地原始请求/响应、截图、诊断摘要与复现实验脚本保存在忽略目录
`artifacts/capability-audit-2026-11-07/`；原生树可能含用户界面内容，不纳入提交。
本次不声称已验收桌面点击、实际文本输入、系统设置修改或跨应用任务完成。

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

## 能力与失败场景

| 场景 | 当前能力与边界 | 判断 |
| --- | --- | --- |
| 标准按钮、菜单、文本框、滚动、窗口切换 | 三端都有语义/原生动作实现；当前 Linux 完整树、焦点、命中与会话条件必须满足 | 条件性支持，不能概括为任意应用 |
| 文件管理器/编辑器的大型树 | 本机已出现上述整应用阻断 | 当前优先级最高的可用性缺陷 |
| 自绘 UI、Canvas/WebGL、远程桌面图像、游戏/CAD | 可能只有容器节点，甚至没有可操作节点；Linux 指针能力还要求 Focusable | 缺少稳定的视觉目标闭环；不是所有 Canvas 都必然失败 |
| 同一画布框选、地图平移、签名 | 同一 target 作为拖拽两端被统一会话禁止，即使 fractions 不同 | 三个平台 AI 统一入口均受影响 |
| 文件管理器拖文件到另一应用 | 两端必须同一 resource / observation；Windows driver 另明确禁止跨窗口 | 当前动作不能表达；可替代上传/复制路径只覆盖部分任务 |
| Ctrl+click 多选、Ctrl+drag 复制、长按、曲线/套索 | AI Action 不含 modifiers、持续按住、路径或独立 down/up；Key 会完整释放按键 | 复合手势不足，不是禁止裸坐标的必然后果 |
| 密码登录与授权凭据输入 | Browser / Linux / macOS 禁止 protected 元素写入；Windows 允许保密写入 | 平台策略不一致；已有登录态/人工输入只能覆盖部分流程 |
| Chromium 业务网页 | 已有 iframe/OOPIF、open shadow、select/check、标签页/弹窗、JS dialogs、上传、下载 | 普通 DOM 流程较完整；本次成功动作回归仍待租约释放 |
| 大型网页 | 上限 2000；截断后整页拒绝；scope 仅支持唯一 HTML id | 本地 2100 行页面已复现 |
| 动态 scoped 网页 | scope 实际监听整个 document，范围外 DOM 变化也使目标失效 | 代码与 Node 最小模型确认；[待验证] 真实浏览器释放租约后复核 |
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

1. **P0：恢复普通应用可用性。** 正确处理 AT-SPI null 引用；暴露截断原因；提供按已观察窗口/容器进行完整局部观察。
   在当前 Dolphin、VS Code、Codex 上验收观察→定位→非破坏性动作→实际回读；不能只扩大节点预算。
2. **P0：使输入占用可诊断、可释放，并隔离独立执行环境。** 保留共享前台的互斥，研究独立 headless browser instance
   的租约范围；提供占用原因与正常关闭/释放路径。验收旧会话存在时独立 headless 流程仍可运行，真实桌面仍不会被并发输入。
3. **P1：让视觉观察能转成受控操作。** 支持已观察 surface 内的 fraction 输入、同元素拖动、modifier click/drag、
   有界长按和路径；保持窗口/元素身份、时效、命中复核与动作后观察。不要用裸坐标跳过观察。
4. **P1：分离保密读取和授权输入。** 设计明确授权、日志脱敏的 secret 输入；统一三端与 browser 策略。
   不以可读出密码作为可写入凭据的前提。普通页面快捷键也需要与浏览器级动作分别设计。
5. **P1：修正 scoped DOM 失效范围与 macOS drag。** 局部观察只因与身份/位置相关的变化而失效；保留必要的祖先/layout
   校验。macOS 修复需真实效果验收，不能以 Linux 测试替代。
6. **P2：扩大系统与应用覆盖。** 明确 Wayland/其他 Linux 桌面路线，补 closed shadow / 自绘 / 远程桌面恢复策略；
   Windows、macOS、混合 DPI、复杂 IME、系统权限弹窗分别建立真实验收矩阵。

下一轮本机串行验收：专用编辑窗口输入中文并保存/回读临时文件；文件管理器临时目录新建/重命名/撤销；
系统设置只做搜索、打开分类、返回；浏览器合成页面完成表单、文件上传下载、弹窗、iframe、画布和多选拖动；
每步分别记录派发结果与实际界面/文件状态。桌面输入租约释放前，该正向点击验收仍未完成。
