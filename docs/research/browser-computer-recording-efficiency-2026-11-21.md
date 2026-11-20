# 长流程自动化：批量执行、录制密度与 browser / computer use 能力差距

核查日期：2026-11-21。仓库基线：`origin/main` 的 `1aafe86`；本文“现在”包含本地 `f510718`、`560406c`、`19be8e9`、`ba5dcea`。**实测**指本次运行；**实现**指源码或协议可证，不等于所有环境都实测；**竞品**指厂商一手文档，不是同机跑分；**建议**不是已经提供的接口。这不是厂商成功率、价格或性能排名。

## 1. 先做哪个决定

1. **让模型只在需要判断时回来。** 探索页面用单步；在一个页面核对多个字段用批量查询；已经知道要做什么，用一次 `run_steps` 在本机顺序执行；经常重复的任务保存后按名运行。每次写之前仍重新观察，不跨步骤缓存写入句柄。10 次真实浏览器点击在本机从 30 次 MCP 往返降到 1 次，而且最终状态相同。
2. **长录制的主要问题不是“行数少”，而是“操作写得很多，意图和结果写得很少”。** 100 步的合成样本，录制文件已经有 1,410 行；运行用文件有 2,826 行。把一个“保存”按钮重复描述一百次，不如多说一句“必须出现哪个订单号，超时后能否再提交”。应该压缩重复描述，增加可验证结果，不是增加截屏频率或把 JSON 换成 YAML。
3. **浏览器“能跑脚本”有三种不同权限。** AAD 可执行声明式步骤；驱动内部能运行页面 JS；普通 MCP 客户端不能提交任意 JS、Playwright 或主机脚本。为效率不必开放第三种权限。下一步最值得补的是批量提取结构化结果、富断言和失败定位，而不是一个万能 `eval`。

## 2. 批量能力与实测

| 场景 | 现在怎么做 | 节省什么、没节省什么 |
|---|---|---|
| 不知道下一步点什么 | `control_targets` → `control_observe`（优先 outline / 局部 scope）→ 根据返回节点直接 `control_act`，或先 `control_find` | 由模型处理未知情况；已有目标时不是每步都必须 `find`。动作后旧观察不能作为下一次写入依据。|
| 同屏核对若干字段 | `control_observe` 一次，`control_find_many` 一次；同一 observation 的 1–32 个定位器共享期限，按输入顺序返回 | 少 N−1 次查找往返；不是同时快照多个窗口，也不是可任意投影的表格查询。不唯一仍报错；`expect: optional` 只允许不存在。|
| 已知的短流程，不想存盘 | 本地未限定 provider/resource 且 `--allow-input` 的 MCP：`run_steps({steps, inputs})`，1–32 步、总执行预算 30 秒 | 一次往返，先编译整批，再按顺序逐步重新选应用、观察、定位、校验和执行。取消是协作式；中途失败停止、返回步骤及错误效果，**已生效步骤不会回滚**。|
| 已知是哪个后端 | 在步骤里写 `app.provider`，如 `browser.cdp`；交互发现也可用 `control_targets({provider:"browser.cdp"})` | 不再每步顺便枚举原生桌面或其他后端；指定后端下的资源仍重新发现、检查唯一性。URL 前缀命中两个标签时应收窄，不选第一个。|
| 反复跑或超过 32 步 | `save_workflow` 一次，此后 `run_workflow(name, inputs)`；录制步骤格式上限 500，和临时批上限不同 | 不必每次把步骤交给模型重写。运行时另有条件、循环、超时和持久执行；**持久模式并不接受任意写操作、敏感数据或脚本**，不能用它保证付款/提交在崩溃后恰好执行一次。|
| 跨资源、读多写少 | 资源分别观察；已知表单的同一快照可查多项，两次输入之间重新观察 | 目前没有“单次观察所有窗口并得到同一时刻的值”的保证；只读并发、同窗多字段投影与独占前台写入应分开设计。|

**测量方法。** Linux、本机 stdio MCP、独立 headless Edge profile、本地 HTTP 页面。每次点击使页面计数和标题改变；测试从独立 CDP 连接读 `window.count`，先得 10，再得 20，而不是相信 MCP 返回的 `succeeded`。交互对照组明确使用 `observe → find → act`，资源已知，不计首次应用发现；批量组包含服务端每步的资源发现。均不计启动、模型推理、请求字节和独立验收读数。每种实现各 5 轮、同轮先交互再批量；样本小，不宣称 P95 或同机严格随机对照。复现入口：[浏览器端到端测试](../../tests/browser/batch.test.mjs)。

| 十次点击 | MCP 调用 | MCP 回复字节合计 | 本机耗时，中位数（五次范围） |
|---|---:|---:|---:|
| 最终版：逐步交互 | 30 | 59,424 | 449 ms（442–459） |
| 仅合并 MCP 调用、尚未限定发现后端 | 1 | 957 | 593 ms（542–622）；同阶段交互中位数 458 ms |
| 最终版：内联批量 + 指定浏览器后端 | 1 | 957 | 155 ms（152–169） |

**解释：** 最终版少 29 次往返、回复体积减少 98.4%，这个夹具的中位耗时减少约 65%（2.9 倍）。中间版本说明“工具数少”不等于“执行快”：原来每个按钮都让服务端遍历不相干的原生后端。若每次工具调用额外有固定等待 `L`，本例仅往返项就相差 `29 × L`；这是模型，不是网络/LLM 实测。别把字节当 token、把 30 次工具调用当 30 次模型推理，也别把浏览器数字外推给 UIA、AX 或远程桌面。多标签、超大树、慢导航和业务等待的收益 **[待验证]**。

**怎么用：** 例如“填申请人 → 选部门 → 预览 → 确认生成了草稿”适合一个批次；若下一步是发给客户，则在预览后停下来，让人或调用方根据实际内容确认。不会因为把四个动作塞进一批就获得更大的授权。该例的可执行 JSON 形状见[控制协议与调用示例](../control-sessions.md)。

## 3. 录制与长流程：稀疏在哪一层

| 产物 | 实际内容 | 对长流程的判断 |
|---|---|---|
| `*.recording.json` | 供人和 GUI 编辑的 `control-steps/1`：步骤 ID、应用选择、元素定位、动作或检查、可选输入和备注 | `save_workflow` 合成 100 个点击：25,964 B / 1,410 行。没有 100 个不同业务阶段；这是**序列化密度**样本，不是物理录制器的事件保真率。|
| `*.workflow.json` | 相同录制编译成运行描述；显式 provider、参数、执行步骤 | 同一样本 58,429 B / 2,826 行；适合机器，不适合作为 100 步的日常审阅界面。CLI 的工作流文件可读 YAML/JSON，但这里默认生成的是 JSON。|
| 浏览器过程录制 | `walkthrough.md` 概览、`steps.jsonl`、截图和按步关联的动作/UI/网络证据；`health.jsonl` 记录溢出/空窗等；`mark` 可人工命名关键时刻 | 原始证据不稀疏。它解释当时发生了什么，不等于已推导出一份具有相同业务结果的可执行脚本。截图有节流去重，快切中间状态不保证齐全。|
| 原生录制 → 跨平台步骤 | Linux AT-SPI / macOS AX 共用的事件转换会把相邻输入归并，识别双击、合并同向滚动、折叠聚焦点击；无法识别的动作有 `skipped`，也有 `dropped` 计数。Windows 有专门的转换路径 | 这段公共逻辑只看**当前事件批次**；“同元素”只比较 role/name。两个订单行各有一个“数量”时，可能错误归并；同一个输入框跨轮询批次时，又可能保留多余动作。这是源码可见的风险，不是已测得的现场发生率。|
| 结果与状态 | 步骤 `check` 支持 `present` 和 `within_ms`；locator 可带 `value`、`states` 和容器，因此能间接表达一些值/状态检查。运行时还有更一般的后置条件、分支与循环 | `已提交` 提示出现，不能替代“订单号匹配、导出文件的行数正确”。录制步骤没有同等易用的命名输出、跨步骤比较、下载/网络结果断言或自动推导的业务分支。这是**语义密度**缺口。|
| 隐私 | 专用保密输入不应序列化；实时 MCP 网络查看与离线取证的脱敏契约不同。**浏览器过程录制默认可保留原始 Cookie、Authorization、请求与响应正文** | 不要直接把整包上传给模型或团队。采集前选择 `--redact-network`，按业务缩小范围；这只是过滤常见字段，不等于任意正文/截图都已匿名。默认可分享摘要应与受限原始证据分开。|

**建议的格式：人读一份意图，机器留一份证据。** 保留已有可执行格式和旧录制；在它之上增加可逆编译的简写层。应用和常用目标可命名、继承；阶段保留前置条件、输出、期望效果和遇错策略。展开后每个写步骤仍带完整定位与策略，原始事件、失败截图等用 `step_id` 引用，不拼进 YAML。只在同一资源、同一稳定身份且没有提交、焦点切换、导航等边界时合并事件；不确定就保留并要求确认，不把“略过”说成“录完”。

```yaml
# 建议的作者视图；不是目前可执行的 YAML 语法
phase: 导出本月订单
app: orders-page
steps:
  - fill: [月份, $month]
  - click: 导出订单
  - verify: {download: orders.csv, required_columns: [订单号, 金额]}
on_unknown_effect: 先检查下载记录，不再点击导出
```

**怎样知道改好了：** 给同一 100 步样本做“简写 → 展开 → 对比语义步骤”的往返测试；让审阅人只看作者视图说出每个阶段的结果、最后一次写入以及能否重试。再用有两个同名输入框、滚动跨批、中文输入、切窗、弹窗和中途断连的真实流程验证；拿应用数据或文件内容对账，不能只看两份序列长度相同。录制格式和隐私细节见[录制文档](../browser-recording.md)、[录制事件转换](../../crates/aad-control/src/recording.rs)、[步骤编译器](../../crates/aad-control/src/steps.rs)、[录制存储](../../crates/aad-runtime/src/recordings.rs)。

## 4. 浏览器到底支持哪种脚本

| 从哪里调用 | 能否执行、在哪儿执行 | 应如何使用 |
|---|---|---|
| Rust 驱动内部 | **能**。CDP `Client::eval` 和协议命令可在浏览器中执行；Firefox 后端也使用 BiDi 的 `script.evaluate` / `script.callFunction` 实现观察和动作 | 这是受信代码的实现能力，不是 MCP 已给模型的权限。[CDP 客户端](../../crates/aad-browser/src/cdp.rs)、[BiDi 后端](../../crates/aad-control/src/browser_bidi.rs)。|
| 公开浏览器 MCP / 普通 CLI | **没有通用的任意页面 JS / Playwright 代码入口。** 有观察、查找、动作、等待、capture、部分浏览器专用读操作 | DOM 点击、DOM 填值内部用脚本，不代表客户端能上传函数。用 `run_steps` 替代十几次往返时也没有给客户端新的页面或主机权限。[工具目录](../../crates/aad-mcp/src/control_tools.rs)。|
| MCP 的声明式脚本 | **能**。短流程 `run_steps`；复用 `save_workflow` / `run_workflow`。内联仅受限步骤，不接受任意工作流、Python/JS 或 shell | 适合业务可预测的输入/等待。MCP 也拒绝按名执行含 `script` 的存量流程；阶段中的动态结构化读取仍是缺口。[工作流工具实现](../../crates/aad-mcp/src/tools.rs)。|
| 人工启动的工作流 `script` | **有，但不是浏览器 JS**。`aad run <file> --allow-scripts` 是显式授权的工作流脚本执行路径；Python 沙箱能力取决于运行环境 | 适合受信任的计算/变换；它不会给代码自动注入当前 `page`，不能当作公开浏览器脚本功能。普通 MCP 不能借这个入口运行存量脚本。[脚本执行器](../../crates/aad-runtime/src/script.rs)。|

## 5. Browser use：逐项对比

下表分别选测试框架、MCP、代码型 Agent、语义自动化和托管浏览器作为参照；它们不是功能相同的产品。差距列描述**对本项目用户的影响**，不是每家都有每项。

| 能力 | AAD 现在 | 市场参照（官方） | 差距与决策 |
|---|---|---|---|
| 一轮多动作 | 临时批、命名工作流；每步检查目标唯一且仍有效 | [Browser Use Agent][bu] 每轮生成一个 Python 单元，在持久解释器里调用多个浏览器操作；[OpenAI 集成][oai] 区分批量动作与应用自己提供的代码执行 | **已补固定步骤短批**。尚不能在一批中读取动态列表然后基于其值决定下一组写入；先补类型化输出和分支，别用取消校验换速度。|
| 首次探索与后续复用 | 可把明确的 locator + 动作保存成可审阅的流程；没有自动把一串自然语言意图变成已验证的缓存 | [Stagehand 缓存][sh-cache] 首次推理后可复用动作，页面结构大改需关注缓存失效 | 缓存“如何找到元素”和成功条件，而非上次的节点句柄。失效要能看见、可重新发现，不应自动改点近似目标。|
| 多字段与列表提取 | 一次快照可查多个已知定位器；有 outline、节点、scope、OCR 等读取途径 | [Stagehand extract][sh-extract] 可指定结构化输出；[Playwright MCP][pw-mcp] 支持页面表达式 | “核对 8 个已知字段”与“返回订单表所有行、字段有类型且支持翻页”是两个能力，后者尚无同等公开接口。建议先做有界只读投影，带截断及来源。|
| 引擎覆盖 | Chromium CDP 较全；Firefox BiDi 主文档支持一部分可信输入和导航，子 frame 只读；没有同等 WebKit 后端 | [Playwright 浏览器][pw-browsers] 分 Chromium、Firefox、WebKit 测试 | 不要在选用 Firefox 时悄悄退为桌面点击。发布按引擎、版本、动作的能力协商表；按真实客户比例决定 WebKit 优先级。|
| Frame、Shadow DOM、Canvas | Chromium 可进入支持的 frame/OOPIF；open shadow 与 closed shadow 有边界。Canvas 可在可证明的已观察元素内操作，不等于认出图中每个对象 | [Claude Browser][claude-browser] 同时提供引用与视觉动作；[UiPath 定位][ui-target] 有多个定位方法 | 对无语义对象先提供元素内截图/候选区域和人工验证。不要承诺拿到 closed shadow 内部树，也不要开放全屏裸坐标。|
| 输入语义 | CDP 普通 `click` 是 DOM 激活，带按钮/次数/比例的指针动作以及键盘走不同可信输入路径；这些路径并不完全等价 | [Playwright actionability][pw-wait] 按动作检查可见、稳定、接收事件、可用/可编辑等条件 | 页面监听 `isTrusted`、遮罩动画或高度依赖焦点时，不能仅以“返回成功”证明兼容。补真实应用断言以及明确的输入语义说明。|
| 等待与业务断言 | `control_wait`、动作 `verify`、声明式存在/消失、底层运行时后置条件；检查不重发上一个动作 | [Playwright actionability][pw-wait] 将动作就绪检查与自动重试断言分开 | 优先补文本/值/数量、下载完成及多条件组合；可重复的是观察，不是“再点一次提交”。|
| 导航、页面与弹窗 | Chromium 有导航、历史、开关/激活标签和 JS 对话框；Firefox 不是完整对等。按 URL 前缀/标题找页面可能出现多匹配 | [Claude Browser][claude-browser] 用 tab ID 及浏览器状态管理多标签；[Playwright MCP][pw-mcp] 公开标签/交互工具 | 录制跨标签流程缺的是“来源步骤打开哪个新页”这类可移植因果关系，不是再存一个临时 tab ID。|
| 网络调试、测试替身 | 实时 `browser_network` 有容量与正文限制；离线录制可更详细。实时主页面的列表不是所有 OOPIF 请求的完整清单 | [Playwright MCP][pw-mcp] 将网络检查及路由模拟作为工具；[Browserbase 回放][bb] 可联动观察会话 | 观察请求和拦截/改写请求权限不同。业务操作先补只读条件（例如导出任务已完成），测试环境的模拟单列，不随生产默认开启。|
| 文件、登录与隔离 | 有上传目录授权、下载跟踪、存储元信息、profile 复用和租约。复制已有 profile 不等于完整迁移 localStorage / IndexedDB | [Claude Browser][claude-browser] 要求上传限定范围；[Playwright codegen][pw-gen] 支持保存/加载认证状态 | 首要缺口是把“本次点击 → 正确下载 → 校验内容”连成可验证步骤；不要默认暴露整机文件、Cookie 或凭据。|
| 任意代码执行 | 驱动内部有 JS，普通客户端无通用 eval；提供受限声明式短批 | [Playwright MCP][pw-mcp] 区分页面表达式和运行在**服务器进程**的 `browser_run_code_unsafe`；[Claude Browser][claude-browser] 的可选 JS 在页面权限下运行 | “允许脚本”不是一个开关。页面上下文、宿主运行时、声明式动作要分权限；脚本能读写状态，不能仅因返回 JSON 就认定只读。|
| 录制、回放和定位失败 | 有语义步骤、截图、网络、健康记录和 Markdown；并非所有证据都可直接变为重放步骤 | [Playwright codegen][pw-gen] 生成动作/断言；[Trace Viewer][pw-trace] 用前后快照定位行为；[Browserbase][bb] 有托管会话回放 | 优先做步骤对齐的本地查看器：点失败步骤可看前后 UI、关联请求、独立断言以及录制缺口，而不是默认把全视频送进模型。|
| 长上下文与部分结果 | 本地文件、命名流程和运行状态可复用；一次内联请求不是持久会话或自动总结器 | [Browser Use 长任务][bu-long] 明确预算、部分结果、证据存档和上下文压缩 | 对 100+ 步按阶段返回关键输出和失败窗口；让人或模型按 ID 拉原始证据。存档不等于可以随意恢复不幂等写入。|

## 6. Computer use：逐项对比

| 能力 | AAD 现在 | 市场参照（官方） | 差距与决策 |
|---|---|---|---|
| 桌面驱动与平台成熟度 | Windows UIA、Linux AT-SPI / X11、macOS AX 已各归本系统包；类型统一不等于成熟度统一。Wayland 的授权输入管线尚未接成可操作后端 | [OpenAI][oai]、[Claude Computer][claude-computer] 的模型接口与负责真实执行的环境/驱动是不同层 | 优先做按系统、桌面会话、控件技术和权限组合的效果回归。Linux X11 的成功不能替代 Wayland、Mac 权限弹窗或 Windows 混合 DPI。|
| 原生控件与视觉兜底 | 有语义树、局部图、元素内比例和受控 OCR；屏幕坐标不能由 MCP 客户端直接提交 | [Claude Computer][claude-computer] / [OpenAI][oai-guide] 可依据截图发出指针、拖动、滚动等；[UiPath][ui-modern] 结合语义与视觉定位 | 自绘 ERP、远程画面、图标无名称时，视觉产品更容易给出候选。对 AAD，建议候选必须绑定到已验证的容器、图像及其当前版本；不确定则停。|
| 目标稳定性与误点 | locator 支持角色、名称、automation_id、within、near 等；但原生录制步骤不总能采到这些上下文 | [UiPath 高级定位][ui-target] 组合严格/模糊选择器、图像、锚点等定位方法 | 编辑器应列出候选及区分理由，录制时优先采业务行锚点。模糊命中可以辅助修复，不能单独授权对相似记录执行删除/发送。|
| 键盘、鼠标与焦点 | 有锚定的输入、滚动、拖动、受限全局快捷键；部分跨资源拖动需两端、租约和后端共同支持 | [Claude Computer][claude-computer] 的通用键鼠动作覆盖许多无语义 UI；[OpenAI 集成][oai] 要求执行侧映射动作并处理许可 | 差距在可验证效果和应用兼容，不是缺一个 `click` 名字。中文 IME、多屏缩放、焦点抢占、取消后按键释放都要单独验证。|
| 浏览器 ↔ 桌面混合录制 | 同一声明式步骤格式可命名不同 provider，能手工组合；单页浏览器证据与原生资源录制有各自采集范围 | [Power Automate Recorder][pad-record] 在同一设计器中录制 Web 和桌面操作 | 常见断点：网页下载 → 文件管理器定位 → 表格应用校验。要自动串起来需跟踪资源切换、文件身份、弹窗和统一时间轴；支持两类驱动不等于一键录好全链路。|
| 脚本化与可维护性 | 有人能读的步骤、命名工作流和底层控制流；短批用于确定性操作。高级运行描述和录制作者视图的表达能力有落差 | [Power Automate 子流程调用][pad-subflow] 可复用流程并传递参数；[Browser Use][bu] 用代码变量保持上下文 | 高复用部分应有明确输入/输出、版本、可测试的子流程契约；生成一份巨大展开文件并不能替代这一层。|
| 失败、恢复、重复副作用 | 记录已执行步骤、失败位置、效果；有持久日志、暂停/取消等，但写流程不能默认断点续跑 | [Power Automate 错误处理][pad-errors] 区分动作与块的失败处理；[Browser Use 长任务][bu-long] 支持部分结果 | 设计“草稿已保存”“文件已落盘”这类可独立查证的检查点；读可重试，写到一半必须先对账。不能把上次按钮超时当成上次没提交。|
| 无人值守与多机调度 | 有本机、远端连接和输入租约；不同后端、前台或隔离浏览器实例的并发条件不同。没有证据表明本仓库交付了托管桌面池 | [Power Automate unattended][pad-unattended] 有受管理 Windows 登录/RDP 会话，受机器和授权条件限制 | 远程能连通 ≠ 能创建可交互会话、排队、分配机器并隔离凭据。先做单机可诊断的排队/租约，机群为独立项目。|
| 人接管与高风险动作 | 有权限开关、受限资源、失效拒绝和会话控制；“允许输入”本身不是对某次下单/发送内容的业务审批 | [OpenAI 集成][oai] 示范由应用执行、核对提交内容并取得同意 | 对不可逆操作建议专门的预览/确认点：谁确认、哪份业务数据、确认后页面变化如何作废；无法证明时不要靠自动重试。|
| 桌面可观测性 | 原生/浏览器有事件、状态或截图；统一失败响应与若干真机脚本可用，但没有贯通三系统的业务成功率基准 | [Playwright Trace][pw-trace] 是浏览器测试调试参照，[Browserbase][bb] 是托管浏览器回放参照，不是桌面驱动 | 按步骤记录“发现/观察/定位/校验/输入/等待”分段耗时，加应用侧最终结果。否则只看 `applied` 和总秒数，无法知道是模型慢、枚举慢还是网页真的还没完成。|

## 7. 建议路线与验收门槛（未实现项）

| 顺序 | 交付物 | 如何判定值得上线 |
|---|---|---|
| P0：让短批好用 | 在客户端按“未知单步、同屏多查、确定短批、复用命名流程”选择入口；增加分段计时、调用数、请求/回复字节及失败步骤 | 10 次确定动作 ≤1 次执行调用；独立读取结果相同；已提交动作绝不因后续失败而重发。多标签、慢站、远端、原生应用另测，中位数和 P95 分开报。|
| P0：让录制可信 | 录制时采稳定上下文，跨轮询批有界去重；突出 `skipped`、`dropped`、不完整阶段；可分享证据包默认最小化、脱敏 | 两个同名行控件不会合并错；同一输入跨批保留最终值；断连后显示缺口；向共享包植入测试密钥，验证摘要、原始引用、截图和正文的披露边界。|
| P0：提高语义密度 | 简写作者视图 + 可逆展开 + 业务断言；允许结果命名以及从已观察数据做受限读取 | 30 步表单能从应用数据复核；100 步流程展开等价，作者视图行数相对当前录制至少减半（目标值）；编译错误不产生任何输入。|
| P1：有界数据处理 | 批量字段投影、表格/列表读取、类型检查、有限循环；保留 scope、字段和输出大小限制 | 分页、虚拟列表、重复列名、缺失/受保护字段均有明确语义；不能把“只见一页”报成“全表”。以导出文件/应用数据做基准。|
| P1：可以解释的恢复 | 按阶段存输入摘要、产物引用和核对步骤；在明确效果后继续，未知效果移交人工 | 100 步混合任务在第 53 步分别注入“未提交”“提交后断连”“只读超时”；已完成业务记录不重复，新一轮能解释从何处开始。|
| P1：平台验收 | Windows 原生窗口、macOS AX 权限与输入、Firefox 子 frame；Wayland 单列授权实现而非写兼容标签 | 同一小任务在支持环境有原生应用或磁盘回读；不支持的动作明确拒绝。各桌面一次只跑一个会占用前台的测试。|
| P2：再决定是否要通用代码/托管 | 有隔离 profile、网络/文件边界与审计要求的场景才评估单独的受信代码执行器；跨机无人值守先定义调度与凭据责任 | 先用 500 步真实任务统计哪些缺口确实无法用类型化步骤补齐；再比较工程成本、人工介入次数、重复副作用、隔离成本，不以竞品有按钮为立项依据。|

**验证边界。** 本轮实测了真实 headless Chromium 内核浏览器及隔离 X11/Qt/AT-SPI 夹具，Windows 平台代码做了交叉检查。**[待验证]** Windows 交互桌面、多 DPI、macOS SDK/Swift/AX/TCC 输入效果、不同图形栈、生产网站和真实长流程；没有跑竞品同机对照。详见[既有平台验收矩阵](../testing/platform-capability-matrix.md)、[上次审计](computer-browser-capability-audit-2026-11-07.md)、[Linux 原生夹具](../../tests/linux/run-rust-atspi-fixture.sh)。

### 官方来源索引

- Browser Use：[Agent 与代码执行单元][bu]、[长任务与检查点][bu-long]。
- Playwright：[MCP 工具与权限][pw-mcp]、[动作就绪条件][pw-wait]、[引擎][pw-browsers]、[录制代码][pw-gen]、[Trace Viewer][pw-trace]。
- Stagehand / Browserbase：[动作缓存][sh-cache]、[结构化提取][sh-extract]、[托管会话回放][bb]。
- OpenAI / Anthropic：[Computer use][oai-guide]、[驱动、批量及代码集成][oai]、[Claude Computer][claude-computer]、[Claude Browser][claude-browser]。
- Microsoft / UiPath：[混合录制][pad-record]、[无人值守][pad-unattended]、[错误处理][pad-errors]、[调用子流程][pad-subflow]、[现代 UI 自动化][ui-modern]、[高级定位描述][ui-target]。

[bu]: https://docs.browser-use.com/open-source/agent
[bu-long]: https://docs.browser-use.com/open-source/long-tasks
[pw-mcp]: https://github.com/microsoft/playwright-mcp
[pw-wait]: https://playwright.dev/docs/actionability
[pw-browsers]: https://playwright.dev/docs/browsers
[pw-gen]: https://playwright.dev/docs/codegen
[pw-trace]: https://playwright.dev/docs/trace-viewer-intro
[sh-cache]: https://docs.stagehand.dev/v3/best-practices/caching
[sh-extract]: https://docs.stagehand.dev/v3/basics/extract
[bb]: https://docs.browserbase.com/platform/browser/observability/session-replay
[oai-guide]: https://developers.openai.com/api/docs/guides/tools-computer-use
[oai]: https://developers.openai.com/api/docs/guides/tools-computer-use-integration
[claude-computer]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/computer-use-tool
[claude-browser]: https://platform.claude.com/docs/en/agents-and-tools/tool-use/browser-use-tool
[pad-record]: https://learn.microsoft.com/en-us/power-automate/desktop-flows/recording-flow
[pad-unattended]: https://learn.microsoft.com/en-us/power-automate/desktop-flows/run-unattended-desktop-flows
[pad-errors]: https://learn.microsoft.com/en-us/power-automate/desktop-flows/errors
[pad-subflow]: https://learn.microsoft.com/en-us/power-automate/desktop-flows/how-to/run-desktop-flow-action
[ui-modern]: https://docs.uipath.com/activities/other/latest/ui-automation/about-the-ui-automation-next-activities-pack
[ui-target]: https://docs.uipath.com/activities/other/latest/ui-automation/advanced-descriptor-configuration
