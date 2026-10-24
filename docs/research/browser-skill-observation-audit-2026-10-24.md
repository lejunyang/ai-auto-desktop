# 浏览器观测评审：从活动日志到 skill 编写证据

日期：2026-10-24。审查基线：`9d50343`。本次交付是代码评审与实施方案，未修改运行代码。

后续实施：2026-10-24 已落地浏览器任务录制，实际入口、范围与验证见 [使用文档](../browser-recording.md)。用户明确修订网络策略为“默认不脱敏，只允许可选参数”，下文相关建议已同步；问题列表与反例仍描述上述历史审查基线，不能作为当前代码的验收结论。

结论：现有 `aad watch` 适合回答“桌面/页面大致发生了什么”，不足以可靠回答“为完成一个任务，用户按什么顺序操作、各步获得什么信息、怎样确认成功”。应保留桌面 ambient observation，新增**有起止范围、以操作步骤为中心的浏览器任务录制**。不要把更多原始事件直接加入现有 JSONL。

证据等级：下文“代码确认”指当前实现；“复现”指提取当前注入脚本进行的 Node 沙箱实验，不代表真人 GUI 实测；所有新设计、性能目标与跨页行为均为 `[待验证]`，所需环境见 §8。仓库内未发现可用于本次统计的历史 watch JSONL，不能给出真实日志压缩率。

## 1. 当前已有能力与可复用部分

| 管线 | 已有实现 | 对本次需求的边界 |
| --- | --- | --- |
| 桌面常驻观测 | `aad-uia/src/observer_win.rs`、`observe_session.rs`、`observe.rs`：WinEvent/UIA 通知、有界缓冲、丢失计数、树差分与内容 digest | 原生 observe 目前仅 Windows；事件唤醒 + 快照确认值得保留，但不提供浏览器网络与任务步骤 |
| 浏览器常驻观测 | `aad-cdp/src/lib.rs`、`aad-cli/src/cdp_watch.rs`：被动 click/input、页面 innerText、文本行集合差分 | 没有网络采集、关键元素语义差分、截图与步骤关联 |
| 浏览器控制 | `aad-control/src/browser.rs`、`browser.js`：显式 page/frame、隔离 world、元素观察与操作前校验 | 可复用资源身份、上下文生命周期经验；这是请求式控制，不是持续录制。不能直接将控制快照 ID 当跨文档的持久 locator |
| 录制/回放 | `gui/src/recording.ts`、recording 规范：locator、参数、结果断言、重新查找 | 可复用 skill 中“目标如何查找、成功怎样判断”的概念；不应把诊断日志或未经确认的推断自动编译成可执行步骤 |

现有桌面 digest 已过滤匿名结构噪声并限制每类条数；浏览器 CDP 路径提前 `continue`，没有使用这套 digest 的 `--max-items` 限制。旧文档中的 604KB → 23–36KB 是 9 月桌面场景证据，不能外推到当前浏览器任务。

## 2. 需要优先修正的具体问题

### P1：操作顺序、身份与完整性不可靠

1. **归并改变顺序并删除真实操作。** `aad-cdp/src/lib.rs::DRAIN_JS` 最后返回 `clicks.concat(byField.values())`：输入→提交会输出提交→输入；同一批两次点击同一开关只留一次；输入以 `tag/type/label/url` 作键，两个无标签输入框会被并成一个。上述三项已复现。动作记录必须保序；只允许合并同一元素、同一连续编辑段内的输入，遇 click/submit/navigation 等边界就结束编辑段。
2. **跳转会清空尚未读取的操作。** 事件只存于页面 `window.__aadCdp`，轮询时才取出；文档销毁前未 drain 的点击/输入没有宿主端证据。`ensure_installed` 在后续轮询才安装，不能覆盖新文档早期动作。代码确认存在丢失窗口；具体丢失率 `[待验证：Windows 浏览器真人点击同步跳转]`。
3. **CDP 异步协议事件被丢弃。** `Client::send` 接收到非当前命令响应时直接继续；注释说会缓冲，实际 `WebSocket` 只持有 `stream`，没有协议事件队列。直接增加 `Network.enable` 不能形成可靠网络采集。需要一个持续接收、分发命令响应和异步事件的连接层。
4. **失败与没有操作混在一起。** `CdpWatcher::observe` 用 `.ok()?` 静默回退；drain 使用 `unwrap_or_default`；已有连接失败后未移除并重连。`Client::eval` 没检查 `exceptionDetails`，反序列化失败也可能变成空列表。应分别报告 unavailable、gap、reconnected、parse_error，不能把它们算作“没有变化”。

### P1：日志可能来自错误页面，且关键变化不可见

5. **标题启发式不能确认目标。** `pick_page` 使用标题前缀，找不到则选第一个 page；多个同名页面或日常浏览器与调试实例并存时会误归属。`/json/list` 第一项不能当作前台标签保证。需要明确选择 page/录制标签，记录 page/frame/document 身份；不确定时暂停归属并报告，不能默认挑一个。
6. **文字差分不是界面语义差分。** `STATE_JS` 只有正文文本：禁用→可用、选中→未选中、图片/Canvas 二维码出现、相同文字的不同元素、列表重排均可能看不见。第一帧会把最多 20 万字符作为 added_text 输出；此后页脚时钟等也可不断产生差分。URL/title 变化但文字和活动不变时，CLI 的 `any` 判定甚至不输出页面变化。
7. **主文档监听不覆盖完整登录路径。** 当前未建立子 frame/OOPIF、popup、新标签、worker 的事件管理；iframe 中的操作不跨文档冒泡到父 document。SSO、跨域二维码、第三方登录必须显式纳入录制范围，不能以“主页面没有变化”宣告流程完成。

### P2：生命周期、输出与入口缺陷

8. **浏览器页内队列无上限。** `q.push` 无容量/字节限制；已复现连续 10000 次 input 全部保留，归并只在 drain 才发生。切走标签后它仍继续收集但不被当前前台轮询及时 drain；stop 也未卸载监听器。长录制需有界队列、溢出计数、stop/disconnect 清理。
9. **截断和写入失败不可见。** input 截 500 字符、正文截 200000 字符，没有截断元数据；已复现正文截断无标记。`run_watch` 忽略 `writeln!`/`flush` 错误，可能报告成功却没保存完整记录。宿主事件时间、序号、录制健康状态也不足以关联请求和界面。
10. **CDP 分支跳过窗口切换处理。** `run_watch` 已取走 UIA 事件后，在 CDP 成功时直接 `continue`，后面的 `foreground_changed` 输出被跳过；`--window` 则完全不走 CDP。行为与“UIA 保留应用切换、CDP 描述内容”的设计不一致。
11. **浏览器录制被 Windows 桌面会话绑住。** `run_watch` 先 `observe_start`，非 Windows 会返回 unsupported；同时进程识别只含 `.exe`。单独 CDP 录制入口应能独立于 UIA 启动。
12. **启动器与现有文档不一致。** `browser_launch::ensure_browser` 对 login 模式传 `--detached --copy-login`，但 helper 在缺少 `--keep-copy`/显式 profile 时拒绝该组合；普通 `--browser chrome` 也未向 helper 传浏览器选择，helper 按 Edge/Chrome/Brave 安装顺序查找。不能将现有“一条命令复用登录态”文档当作当前可用保证。不要为了修复启动而静默增加永久凭据副本。

此外：`observe_session::poll` 的 `at` 是 poll 时间，原始 `observed_at` 不序列化；因此桌面事件也不能直接作为精确因果时钟。原生会话表与基线表的长期容量/回收值得另行补齐，但不应阻塞浏览器任务录制的独立设计。

## 3. 面向用户的结果：一次操作是一条可解释步骤

建议新增概念性入口（**以下命令尚未实现**）：

```text
aad browser-record start --cdp-port 9222 --page <page-id> --out <new-session-dir>
aad browser-record mark --session <id> --note "这里拿到了订单号"
aad browser-record stop --session <id>
aad browser-record export --session <id> --format skill-evidence
```

明确录制的网站、标签和起止时间。mark 是可选语义注释，用于标明“我要学的是哪一步、哪项信息”；无需每步询问。录制默认只读，不改变缓存、拦截/重发请求或替用户点击。

主产物建议：

```text
session/
  manifest.json          # 版本、范围、策略、浏览器版本、能力覆盖、健康/丢失统计
  steps.jsonl            # 小体积结构化步骤；step_id 是引用主键
  walkthrough.md         # AI/人优先阅读：怎么操作、如何等待、拿到什么
  evidence/network.jsonl # 请求证据（默认原值，脱敏可选），按 request_ref 查找
  evidence/ui.jsonl      # 选中区域的基线与语义差分，按 evidence_ref 查找
  images/<hash>.webp     # 关键截图；去重，JSON 中只放引用
```

低层诊断事件可选开启、按字节/时长轮转，不默认无限保存。主产物不是整个 HAR、DOM dump 或 base64 图片的混合。每步包含：前置页面状态、动作与目标 locator 候选、结果/关键变化、取得的信息、关联请求引用、前后截图引用、等待条件、证据引用与不确定性。

示例（虚构站点/端点，仅表示期望输出，并非本次已观测的真实登录）：

```text
1. 在登录页点击“扫码登录”按钮。
   结果：出现“使用手机扫码”对话框及二维码区域。
   请求：POST /auth/qr/create → 200；返回字段 qr_id、expires_in。
   等待条件：扫码对话框可见且二维码区域已渲染。
   证据：ui-02、req-07、shot-02。请求与操作的关联：推断。

2. 等待用户在手机上扫码并确认。
   请求：GET /auth/qr/status 共 18 次，合并显示；状态 waiting → confirmed。
   结果：登录弹窗关闭，页面显示个人菜单。
   成功条件：已登录菜单可见；不能仅凭 HTTP 200 判断成功。
   人工步骤：手机扫码。凭据、二维码载荷不写入 skill。
```

对于“操作哪里获得什么信息”，增加可追溯输出，如 `order_id`：页面上的字段标签/所属结果卡片、示例值的脱敏形式、响应字段路径 `data.order.id`、关联证据；不要仅保留“请求成功”。页面字段与响应字段的对应若未经验证，应标 inferred。

## 4. 采集层：保留原始顺序，先解决可靠性

建议由 `aad-cdp` 承担协议传输，新增 `aad-observe`（名称暂定）承担录制会话、归并与导出，CLI 只负责入口。不要把新逻辑继续塞进 `main.rs::run_watch`。保留旧 watch 输出契约，避免让现有消费者突然读取另一种格式。

1. **单连接读取者与消息分发。** 独立接收循环将响应按 command id 投递，异步事件进入有界队列；消费、落盘、截图不阻塞接收。处理断连、超时、晚到响应、session detach 与退避重连。不要让两个调用方并发读同一个 socket。
2. **尽早安装并即时送到宿主。** 用 `Page.addScriptToEvaluateOnNewDocument` + 隔离 world + `Runtime.addBinding` 上报页面活动；已有文档也安装。监听 click、表单 submit、控件 change、连续输入完成、必要的 Enter 提交与选择变化；不逐键记录全部按键。监听 hover/scroll 时只保留引出菜单或关键区域的语义结果。绑定与脚本均在 stop 时清理。
3. **事件身份与时序。** 每条事件记录宿主 seq/接收单调时间、源时间、session/page/frame/document epoch、元素 ID；CDP 请求另带 requestId/loaderId/redirect hop。浏览器 performance 时间与 CDP 单调时间做映射，保留不确定度，不把墙钟排序当因果证明。页内事件保留 `isTrusted`，它仅是来源提示；页面内容和事件都是不可信输入，不能变成 agent 指令。
4. **目标生命周期。** 初始 page 显式选择；订阅 target/frame/context 生命周期，按会话范围跟踪 opener 派生的登录 popup 与授权跳转，记录 page 关系。跨域/OOPIF 需独立 session 管理。扩展、其他无关标签默认不采。worker/service-worker 网络覆盖不齐时明确标 coverage gap，首版不宣称全覆盖。
5. **不要承诺零丢失。** 即时 binding 降低导航前丢失概率，仍需 seq、溢出计数、断连空窗与文档切换标记；浏览器崩溃、快速同步导航、受限页面仍需实测。

相关协议能力以官方 [Page](https://chromedevtools.github.io/devtools-protocol/tot/Page/)、[Runtime](https://chromedevtools.github.io/devtools-protocol/tot/Runtime/)、[Target](https://chromedevtools.github.io/devtools-protocol/tot/Target/) 为依据。具体浏览器版本支持需运行时探测；以上组合是本项目建议，不是协议提供的完整录制器。

## 5. 网络：采摘要、关联步骤、折叠重复，不按类型一刀切

通过 CDP [Network](https://chromedevtools.github.io/devtools-protocol/tot/Network/) 事件建立 request → response → finished/failed 生命周期，必要时取有限响应体；不 monkey-patch fetch，不使用请求拦截作为默认采集手段。主要字段：方法、脱敏 URL、类型、状态、耗时、frame/loader、initiator 摘要、缓存/worker 来源、请求参数结构、响应类型与关键业务字段。

| 请求类别 | 默认处理 | 必须保留的例外 |
| --- | --- | --- |
| Document、登录重定向、XHR/Fetch | 保留元数据；任务相关请求额外保留有限脱敏 JSON/文本与字段结构 | 跨域认证请求不能因第三方域名被过滤 |
| 字体、CSS、脚本、普通图片/媒体 | 摘要层只计数/字节，不存 body | 错误、下载、关键元素引用的图片/二维码资源保留引用与元数据 |
| 统计、心跳、推荐刷新 | 经规则识别后在主摘要降权/隐藏 | 不靠 URL 含 `track` 等单一关键词硬删除；人工可标 keep |
| 同端点轮询 | 按 page/frame、方法、路由、参数形状/业务键分组，记次数与首末时间 | 保留业务状态改变、错误、首末样本；HTTP 200 不代表业务状态相同 |
| WebSocket/SSE | 连接与消息计数；按配置保留状态变化摘要 | 扫码完成若依赖推送，必须有状态转换证据；暂不支持要明确报告 |

识别“无用”采用分层规则：先限录制范围，再按静态类型/明确站点规则降噪，最后结合动作时间窗、frame、initiator、关键元素资源引用与结果字段评分。`kept/aggregated/omitted` 都记录原因和计数。未知相关性请求保留紧凑元数据，避免在采集端不可逆删除未来可能需要的证据。

请求归属是**关联推断**：动作后一段时间出现的请求可能是后台刷新；异步请求也可能晚于下一次操作。保留关联理由、置信度与 unassigned，不强行每条归入最近点击。redirect hop 不覆盖前一跳；请求完成前不读取响应体；流式、二进制、取 body 失败、截断分别标记。

网络默认保留采集到的原值。仅在显式开启 `--redact-network` 时，才对 Cookie/Set-Cookie/Authorization、URL query、postData、响应 JSON 中的常见敏感字段使用占位符；该开关不等同于截图或页面内容过滤。保留业务字段名/类型与业务值，使 AI 能理解“得到了什么”。实际生成可复用 skill 时应将动态参数外置，录制证据与可执行 skill 的数据策略分开。

## 6. 页面与截图：记录决策相关变化

**关键元素模型。** 记录目标及其局部上下文：role、可访问名称、label、稳定 test id/属性候选、frame 路径、可见/可用/选中/展开/焦点状态、必要 value；配套弹窗、错误提示、结果卡片、表格关键列、链接/下载信息。展示用途身份与可回放 locator 分开。对重名元素保留祖先区域与匹配数，不能假装唯一。

**变化确认。** MutationObserver 只负责标脏区域，限频读取局部 DOM/AX 语义快照再比较；不逐条输出 mutation。保留 actionable 状态变化、关键文本/业务数据变化、导航、对话框、错误；折叠同一节点的重复渲染，忽略无业务意义的 class/style/动画变化。不能直接忽略全部 visibility/style，因为它可能正是登录弹窗出现的信号。Canvas/图片内部变化不靠 DOM mutation 就能完整覆盖，需关键检查点截图或局部视觉校验，无法识别则标 unknown。

**形成步骤。** action 打开步骤，聚合相关变化；下一动作、关键结果或短暂静默结束步骤。连续打字按稳定元素 ID 归并，跨点击/提交不归并。采用静默窗口 + 最大等待时间，不能只等 network idle（扫码轮询可能永远不空闲）；最大等待到达标 pending，后来的结果仍可关联原步骤。无用户动作的状态变化单列 system transition。mark 可补充用户意图，但不替代证据。

**关键截图。** 会话基线、操作后的主要状态、弹窗/二维码出现、导航完成、错误或人工 mark 时截图；不在每次 DOM 变更或轮询请求时截图。截图是独立文件、内容 hash 去重、优先 viewport + 关键区域裁剪。只用最后一个已确认状态作 before 图；若未及时拍到，写 missing，不能把点击后的图冒充点击前。记录 capture 时间/页面/文档身份和相对动作的延迟，导航期间若身份不一致则丢弃或标不确定。

二维码本身是短效登录材料：对 AI 编写 skill，默认截图遮蔽码体并保留位置/周边说明；界面证据仍能证明“这里出现扫码区域”。像素遮蔽失败时不导出该图，明确记缺失。遮蔽副本不修改网页，也不触碰原浏览器输入。普通非敏感业务信息按录制策略保留。

**预算（建议初值，均待基准标定）。** 人读摘要每步目标约 1–2KB/不超过 10 项关键变化，单响应体最多 32KiB、单步骤可取 body 总量 256KiB；每步常态 1–2 张图，按状态转换允许额外图；会话总存储预算可配置（初值 100MiB）。预算到达时降级保留动作、结果、错误与丢失统计，不继续无界增长。大结果可从证据引用按需读取；每次截断必须有总数/原始大小和原因。内容压缩率与 token/step 是验收指标，不是当前已实现的收益。

## 7. 从证据生成 skill，以及实施顺序

`walkthrough.md` 先给 AI；AI 按引用按需读请求/截图，不扫描整场日志。生成的 skill 包含前置条件、站点与起始页面、参数、逐步操作、等待/成功判断、返回信息字段、已观测失败分支、需要用户完成的扫码/确认步骤。只看到一次成功样本时，不编造验证码过期、未授权等分支已经验证；标待补录。录下接口不等于授权直接调用它，接口自动化是后续单独的能力设计。

回放必须重新观察并查找目标，生成当前 `snapshot:revision:node`（或现有控制会话等价的观察/校验引用），执行前再校验。录制 ID、DOM node ID、截图里的坐标均不可直接作为持久执行句柄；MCP schema 继续禁止裸坐标。

| 独立交付 | 改动范围 | 完成门槛 |
| --- | --- | --- |
| A. 修复现有观测正确性 | `aad-cdp` 事件保序/元素身份/有界缓冲/截断错误；`cdp_watch` 重连与页面归属；CLI 写入/切换/启动器问题分独立提交 | 回归复现不再出现；Windows 真实入口验证；不改变旧 JSONL 的基本用途 |
| B. 浏览器任务录制 MVP | 独立 CDP 录制生命周期 + 显式 page + 早期注入 + 网络摘要 + 关键元素 + 截图引用 + steps/walkthrough | 本地完整扫码 fixture；含同步导航和一个跨域 iframe/popup 登录路径，真实目标浏览器验收 |
| C. 降噪与 skill 证据导出 | 请求轮询折叠、业务状态提取、局部语义变化、截图预算/脱敏、按需证据读取 | 对照完整测试 fixture，步骤与关键请求不漏，噪声与字节下降可量化；AI 能据此写出带正确等待条件的 skill |
| D. 日常浏览器低打扰接入 | 评估扩展/用户明确开启的调试实例等接入方式，独立调研 | 不强制关闭用户全部浏览器；明确权限和各浏览器支持范围 |

建议第一版使用独立、可复用的录制 profile，用户在其中正常登录一次；不要把“复制日常登录数据且关闭浏览器”作为默认路径。Chrome 136 起调试默认数据目录受限制，官方建议自定义 user-data-dir；不能把旧文档“同用户复制 cookie 就能保持登录”泛化到当前所有 Chrome/Edge 版本。依据：[Chrome 官方变更说明](https://developer.chrome.com/blog/remote-debugging-port)。具体登录态可用性 `[待验证：目标浏览器版本、平台与企业策略]`。

每个独立功能验证通过立即本地提交；涉及真实 GUI 的验证串行。浏览器 recorder 不依赖 UIA 事件面，但 Windows 当前用户路径仍必须真机验证。

## 8. 验证与本次实际执行记录

### 新设计验收矩阵（尚未执行）

使用只在 loopback 提供的测试网站、隔离 profile 与目标 Chromium；串行进行 Windows 真人/目标环境输入测试。fixture 服务端独立记请求，页面独立报告可见状态，作为录制器之外的 oracle。

- 输入→点击提交、两个同名/无标签输入框、两次点击开关：顺序/次数/身份一致；中文 IME 合并后最终值正确。
- 同步整页跳转、SPA 同文案不同 URL、刷新、同名双标签、SSO popup、跨域 iframe/OOPIF：页面归属正确；漏采必须显式 gap。
- 扫码 fixture：按钮→显示二维码区域→轮询 waiting→确认→个人菜单；网络包括二维码图片、埋点、相同 HTTP 200 不同业务状态，以及备用 SSE/WS 通知。摘要保留真正状态转换，静态资源/埋点只计数。
- 界面只有 disabled/selected 改变、Canvas 码体更新、列表顺序变动、无文字图片出现：不能全部判“无变化”；不能靠文本长度断言正确。
- 截图回读验证能看清按钮/弹窗，码体确实遮蔽；响应/URL/DOM/截图中植入测试 secret，递归检查导出无明文泄漏。
- 断开 CDP、页面异常、慢响应、队列溢出、磁盘写失败、stop 与异常退出：日志报告缺口/失败，停止后监听与本次连接清理，其他浏览器不受影响。
- 长稳和体积：至少 30 分钟含切换/静默/轮询，测队列高水位、CPU、采集延迟、bytes/min、tokens/step、截图数量；与同场景未折叠证据对照。预设关键事件保留清单，压缩不能靠漏掉这些事件达成。

### 本次执行

- `osdk exec --tool rust -- cargo test --locked -p aad-cdp`：8 个库测试 + 1 个启动器测试通过。现有测试通过不覆盖上述行为缺陷。
- 提取 `INSTALL_JS` / `DRAIN_JS` / `STATE_JS` 原文在 Node VM 运行：5 项问题复现（顺序翻转、重复点击丢失、字段合并、10000 条无界积压、200000 字符静默截断）。未触碰用户浏览器/真实登录。
- 没有 Windows 前台 GUI 环境验证，没有跑真实网站登录，也没有实现新的网络/截图录制；不能据此宣称新方案已可用。

最小可复现命令：在仓库根目录，用 PowerShell 7 将以下 JavaScript 保存到临时 `.cjs` 文件并执行 `node <file>`。它读取生产脚本，不另写一份归并实现；这是审查基线的反例，不是期望未来保持的行为。

```javascript
const fs = require('node:fs'), vm = require('node:vm');
const assert = require('node:assert/strict');
const source = fs.readFileSync('crates/aad-cdp/src/lib.rs', 'utf8');
const script = name => source.match(new RegExp(`const ${name}: &str = r#"([\\s\\S]*?)"#;`))[1];
const drain = raw => JSON.parse(JSON.stringify(vm.runInNewContext(
  script('DRAIN_JS'), {window:{__aadCdp:{take:()=>raw}}})));
const input = value => ({kind:'input',tag:'input',type:'text',label:null,value,url:'fixture'});
const click = () => ({kind:'click',tag:'button',label:'submit',href:null,url:'fixture'});
assert.deepEqual(drain([input('alice'),click()]).map(e=>e.kind), ['click','input']);
assert.equal(drain([click(),click()]).length, 1);
assert.equal(drain([input('field-1'),input('field-2')]).length, 1);
const listeners = new Map();
const sandbox = {window:{},document:{addEventListener:(n,f)=>listeners.set(n,f)},location:{href:'fixture'}};
vm.runInNewContext(script('INSTALL_JS'), sandbox);
const element = {tagName:'INPUT',type:'text',value:'x',id:'sample',getAttribute:()=>null};
for (let i=0; i<10000; i++) listeners.get('input')({target:element});
assert.equal(sandbox.window.__aadCdp.take().length, 10000);
const state = vm.runInNewContext(script('STATE_JS'), {
  document:{title:'fixture',body:{innerText:'x'.repeat(200010)}},location:{href:'fixture'}});
assert.equal(state.text.length, 200000);
assert.equal(Object.hasOwn(state,'truncated'), false);
console.log('Confirmed five baseline observation defects; no GUI verification.');
```
