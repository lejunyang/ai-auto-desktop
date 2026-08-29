# 录制回放编排架构

状态：Draft，2026-08-29。数据契约见 `docs/spec/recording-session-v1alpha1.md`。本文档说明进程结构、UI 形态、捕获机制与回放判定，并逐条说明**为什么**这样选，以及在本机实测得到的约束。

## 1. 设计出发点：三条实测约束

设计不是从「录制器一般怎么做」出发，而是从本项目已有契约的实测事实出发。

**（1）driver 是纯请求/响应的，目前不存在任何事件面。** 实测扫描三个 driver，均未出现
`AddAutomationEventHandler` / `AXObserver` / `atspi_event` / `SetWinEventHook` 之类符号。因此录制**不能**靠「给 driver 加个回调」实现——需要新增一个能力，而这个能力必须遵守现有的进程隔离边界。

**（2）target 不可持久化，locator 必须唯一。** 见 spec §1：driver 只保留一个 `self._current` 快照，且对歧义 fail-closed。这决定了录制器的核心职责不是「记下点了哪里」，而是**在录制当场合成一个可证明唯一的 locator**。

**（3）没有可用的 GUI 工具链。** 实测本机：`tkinter` 可 import 但 `Tk()` 运行期失败（Tcl 未正确安装）；PySide6 / PyQt6 / wx 全部缺失；项目当前依赖仅 `PyYAML` `jsonschema` `Pillow`。因此 UI 方案不能建立在任何桌面 GUI 框架上。

## 2. 进程结构

```
                    ┌──────────────────────────┐
   浏览器（本地）──▶ │  recorder UI server      │  stdlib http.server
   loopback+token    │  (host 进程内)           │  无新增依赖
                    └────────────┬─────────────┘
                                 │ 进程内调用
                    ┌────────────▼─────────────┐
                    │  recording orchestrator  │  合成 locator / 验证唯一性
                    │  (host 进程内)           │  编译为 workflow
                    └────────────┬─────────────┘
                                 │ 现有 NDJSON 协议（不新增传输机制）
                    ┌────────────▼─────────────┐
                    │  desktop driver plugin   │  新增 observe 类 action
                    │  (独立进程)              │  受 Job Object / killpg 约束
                    └──────────────────────────┘
```

关键决策：**捕获逻辑放在 driver 进程，不放在 host。** 理由是现有架构已经规定第三方 native provider 必须进程外运行、不得 import/dlopen 进 trusted host。录制需要注册 UIA/AX/AT-SPI 事件回调并长期持有 COM 引用，这正是应当隔离的那类代码。附带收益是：录制进程崩溃不会带走 host，且它的进程树已由本轮实现的 Job Object（Windows）与 `killpg`（POSIX）保证可完整回收。

## 3. 捕获机制：不做键盘钩子

这是本设计最重要的安全决策，且有实测支撑。

两类可选机制：

| | 能看到什么 | 代价 |
| --- | --- | --- |
| A. 全局输入钩子（`SetWindowsHookExW` + `WH_KEYBOARD_LL`、`CGEventTap`、X11 `XRecord`） | 系统内**所有**按键，含在无关应用里输入的密码 | 等于系统级键盘记录器 |
| B. 可访问性事件（UIA `AddFocusChangedEventHandler`、AX `AXObserver`、AT-SPI listener） | 焦点/属性/结构变化，即「哪个元素被激活」 | 看不到原始按键 |

实测验证 B 足够：注册 UIA focus-change handler、**不安装任何键盘钩子**，捕获到的事件已包含完整元素身份——
`{role_id: 50020, name: 'PowerShell 7', class_name: 'TermControl', automation_id: '', process_id: 37496}`。这正是合成 locator 所需的全部字段。

因此规定：**录制器不得安装全局输入钩子。** `SetWindowsHookExW`、`CGEventTap`、`XRecord` 在录制路径中一律禁止。

代价必须诚实说明：放弃钩子意味着**无法逐键还原用户输入的文本**。设计对此的应对是把它变成优点——文本本来就必须外提为 workflow `inputs`（spec §7.3），不该内联字面量。录制器记录的是「向这个元素输入了文本」这一事实与目标 locator，文本值由操作者在 UI 中填写或声明为 input。这既避免了键盘记录，又天然满足脱敏要求。

对于确实需要精确按键序列的场景（快捷键、组合键），走显式路径：操作者在 UI 中手动添加一个 `type_text` 或后续的按键 action 步骤，而不是靠钩子偷录。

## 4. 录制流水线

每个用户动作走同一条流水线，**任何一步失败都不得静默降级**：

```
可访问性事件到达
   │
   ├─▶ 1. 触发 snapshot（driver 既有 action，不新增语义）
   │
   ├─▶ 2. 在快照中定位事件对应节点
   │
   ├─▶ 3. 合成 locator：按 role → +name → +class_name → +automation_id
   │       → +framework_id 逐步收窄，达到唯一即停
   │
   ├─▶ 4. 验证唯一性：实际执行一次解析，确认候选数 == 1
   │       ├─ 唯一        → strategy=unique, verified=true
   │       ├─ 祖先可消歧  → strategy=scoped（必须实测验证祖先确实不同）
   │       ├─ 仅序号可区分→ strategy=ordinal, fragile=true
   │       └─ 无法证明    → strategy=unresolved, enabled=false，要求人工处理
   │
   ├─▶ 5. 脱敏：丢弃 value（默认）、外提文本为 input、丢弃窗口标题
   │
   └─▶ 6. 提议 assertion：为写动作自动提议一条 postcondition
```

第 3 步的收窄顺序与停止条件来自实测（spec §7.2）：`role+name+class_name` 达到 87.0% 唯一率与 100% 跨会话稳定率，而继续追加 `automation_id`+`framework_id` 唯一率仍是 87.0%。**多加字段不提升唯一性，只增加对 UI 变更的脆弱性**，所以必须「达到唯一即停」。

第 4 步的 `scoped` 分支必须实测而非假定。实测反例：`["pane","","Microsoft.UI.Content.DesktopChildSiteBridge"]` 的 3 个同构节点**共享同一父节点**，祖先无法消歧；而 `["pane","","InputSiteWindowClass"]` 的 3 个节点父节点各异，祖先可行。录制器必须真的比较祖先，不能假设「加上父节点就唯一了」。

## 5. UI 形态与选型理由

**选择：host 进程内的 stdlib HTTP 服务 + 系统默认浏览器渲染的单页界面。**

为什么不是桌面 GUI 框架：实测 `tkinter` 在本机运行期即失败，Qt/wx 均不可用；引入其中任何一个都会给一个当前只依赖 PyYAML/jsonschema/Pillow 的项目增加重量级平台相关依赖，且三端打包各不相同。

为什么浏览器方案可行且安全，已实测验证：绑定 `127.0.0.1` + 端口 0（OS 分配）+ `X-Recorder-Token` 头校验。实测结果：带正确 token 返回 200；缺 token 返回 401；从本机非回环地址连接同一端口**超时不可达**。token 放在请求头而非 URL，避免经由浏览器历史与日志泄漏。

补充约束：

- 服务仅在录制/编辑会话期间存活，会话结束立即关闭监听。
- 不提供任何「执行任意 action」的端点。UI 能触发的操作限于：开始/停止录制、编辑步骤、重排、编译、以及**显式**的单步试跑。
- 试跑与正式回放走同一条 workflow 执行路径，接受同样的 policy/risk/confirmation 检查。UI 不是绕过策略的后门。

UI 的四个视图，各自对应一项用户诉求：

| 视图 | 诉求 | 内容 |
| --- | --- | --- |
| 步骤列表 | 可编辑重排 | 拖拽排序、启用/禁用、插入 logic 步骤 |
| 步骤详情 | 修正 locator | 展示合成的 locator、`observed` 证据、候选数、脆弱性标记 |
| 逻辑编辑 | 自定义逻辑 | 条件/循环/赋值/分组的表单式编辑，表达式为完整 `${{ }}` |
| 回放报告 | 判定 | 每步的 assertion 结果与失败归因 |

步骤详情视图必须显示 `disambiguation.strategy` 与候选数——因为 driver 对歧义 fail-closed，「这个 locator 匹配了 3 个节点」是操作者**必须**在录制阶段就看到的信息，而不是等回放时才收到 `DRIVER.AMBIGUOUS`。

## 6. 可编辑重排的语义保证

重排看起来只是改列表顺序，但必须防住三类静默错误。**这一节的分工边界来自实测，不是推测**：把编译产物交给项目自带的 `compile_descriptor()` 做了三组对照实验。

**（a）数据依赖倒置 —— 现有编译器已覆盖。** 实测把 `enter_note__find`（引用
`steps.enter_note__snapshot.output`）从索引 4 移到索引 0，`compile_descriptor()` **在编译期拒绝**：

```
uncovered_step_reference: steps reference 'enter_note__snapshot' is not covered by depends_on
```

因此录制编译器**不需要**自建顺序检查，复用现有 DAG 校验即可。

**（b）引用已删除步骤 —— 现有编译器不覆盖，必须由录制层补。** 这是实测推翻我初始假设的一处。删除
`enter_note__snapshot` 而保留引用它的 `enter_note__find`，`compile_descriptor()` **接受**了该 descriptor，并把
`enter_note__find` 的依赖规范化为 `('focus_editor',)`。把引用改成完全不存在的 `steps.no_such_step` 同样被接受。

原因是现有校验的语义是「引用必须被 `depends_on` **覆盖**」，而非「被引用的 step 必须**存在**」。缺失的 step 不产生依赖，也就无所谓未覆盖。

结论：**录制编译器必须在生成 workflow 之前自行校验引用完整性**——每个 `steps.<id>` 引用与每个
`assertion.of_step` 都必须指向一个存在且 `enabled: true` 的步骤，否则报
`RECORDING.ORDER_INVALID`。这也是 UI 默认提供「禁用」而非「删除」的直接理由：禁用是无损的，且不会产生悬空引用。

**（c）UI 状态前置依赖 —— 无法静态覆盖，靠 assertion 定位。** 步骤 3「点击保存」在录制时成功，是因为步骤 2 打开了对话框。把 3 提到 2 之前，静态检查看不出问题（没有表达式引用），但回放必然失败。

设计对此不假装能自动解决：

- 每个 interaction 步骤都带 assertion（spec §7.4），前置状态不满足时该步 postcondition 失败，给出**明确失败点**而非难以归因的崩溃。
- UI 在检测到重排跨越了「窗口/对话框出现」类 assertion 时给出警告，但不阻止——操作者可能正是有意重构。
- `metadata.annotations["ai-auto-desktop.dev/recorded-order"]` 保留原始录制顺序，UI 标注「已偏离录制顺序」。

## 7. 编译模型（已实测验证）

一条 interaction 不能编译成一个 action。因为 §1 约束（2），写动作需要一个当次会话内有效的 `target`，而录制产物里只有 locator。因此每条 interaction 展开为三步：

```
snapshot  →  find（locator → target）  →  真实动作（用 target）
```

`find` 的输出 target 通过表达式传给动作，snapshot id/revision 通过表达式传给 `find`。这样既满足了 driver 的 `target` 要求，又保证每次 attempt 都是**重新解析**——与现有契约「每次 attempt 必须重新解析 locator、不得跨 snapshot 复用 node_id」完全一致。

assertion 不编译成独立步骤，而是**附加为它所属步骤的 `postcondition`**，其中 `observe` 使用 `find`。这利用了现有的 postcondition 机制，不新增执行语义。

已用原型验证这条链路真的成立，而非纸面设计。一份含三种步骤（interaction / assertion / logic-condition）的录制编译后：

- 通过未经修改的 `schemas/workflow/v1alpha1/workflow.schema.json`；
- 被项目自带的 `ai_auto_desktop.compiler.compile_descriptor()` **接受**；
- 依赖被正确规范化，包括嵌套 scope 内独立成链：

```
focus_editor__snapshot     depends_on=()
focus_editor__find         depends_on=('focus_editor__snapshot',)
focus_editor               depends_on=('focus_editor__find',)
enter_note__snapshot       depends_on=('focus_editor',)
enter_note__find           depends_on=('enter_note__snapshot',)
enter_note                 depends_on=('enter_note__find',)   +postcondition
only_if_requested (if)     depends_on=('enter_note',)
   then: click_save__snapshot depends_on=()
   then: click_save__find     depends_on=('click_save__snapshot',)
   then: click_save           depends_on=('click_save__find',)
```

代价必须说明：一条用户动作变成三个 step，会消耗 `budgets.max_executed_steps`。录制编译器应据此设置合理的默认预算（约为 interaction 数的 3 倍加余量），并在 UI 中显示预算占用，而不是让操作者在回放中途撞上预算耗尽。

## 8. 回放判定：什么算成功

明确规定：**动作被派发不等于回放成功。**

回放复用现有的桌面动作闭环
`observe → resolve → precondition → policy/confirm → execute → re-observe → postcondition`，录制层不新增执行语义。判定规则：

- 每次 attempt 必须重新 `snapshot` 并重新解析 locator。**不得**跨 snapshot 复用 `node_id`——这既是现有契约要求，也是 §1 约束（2）的直接后果。
- 有 assertion 的步骤：assertion 通过才算成功。
- 无 assertion 的步骤：只能报告「已派发」，编译期产出 `RECORDING.ASSERTION_MISSING` 警告。回放报告**不得**把它显示为绿色成功。
- `DRIVER.AMBIGUOUS` / `DRIVER.NOT_FOUND` 必须归因到具体步骤，并把 `observed` 证据与当前快照做差异对比，指出是哪个字段变了。这是 `observed` 存在的主要理由。

失败归因必须结合 `capture.probe`。例如目标应用已提权而录制环境 `windows.integrity` 为 `medium` 时，报告应指出这是 UIPI 阻断而非 locator 失效；`windows.dpi` 为 `degraded` 且 `pointer_quantisation` 为 2.5 时，小目标的指针偏差应归因到坐标量化。

## 9. 跨平台实现分期

各平台的事件面成熟度差异很大，因此分期交付，且**每期都必须在真机验证后才算完成**：

| 期 | 内容 | 前置 |
| --- | --- | --- |
| 1 | Windows：UIA focus/property/structure 事件 → 录制；stdlib UI；编译到 workflow | UIA 事件已实测可用 |
| 2 | 回放判定与失败归因报告 | 期 1 |
| 3 | Linux AT-SPI 事件录制 | 现有 KDE/X11 driver 已验证读写 |
| 4 | macOS AXObserver 录制 | 受 TCC 约束，需真机 TCC 身份 |

不承诺「一次实现三端」。理由与现有支持矩阵一致：macOS 的 TCC 身份绑定可执行文件签名，Linux 结果受发行版/compositor 影响，二者都必须真机验证。跨平台性指录制器与 UI 在三端可运行；录制产物本身绑定录制平台（spec §6），不做跨平台转换。

## 10. 明确不做的事

- 不做全局键盘/鼠标钩子（§3）。
- 不做像素级坐标录制回放。坐标必须由回放时的 fresh 语义节点推导；`observed.bounds` 仅作诊断。
- 不做 workflow → recording 的反向编译（spec §8）。
- 不做「录制即可信」。录制产物经过与手写 workflow 完全相同的校验与策略检查。
- 不在录制路径引入新的 IPC 机制。复用现有 NDJSON + artifact 通道。
- 不默认截图。`redaction.screenshots` 默认 `none`；实测已证明语义树本身就会带出文件正文（spec §5），截图只会放大暴露面。
