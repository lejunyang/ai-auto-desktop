# 录制会话格式 v1alpha1（Recording Session）

状态：Draft，2026-08-29。本文中的“必须”“不得”“应当”具有规范含义。本文档定义**录制产物**的数据契约；它与 `docs/spec/workflow-descriptor-v1alpha1.md` 是两种不同的东西，转换规则见 §7 与 `docs/architecture/record-replay.md`。

## 1. 为什么录制产物不能直接是 Workflow

一个直觉方案是「录制时直接生成 Workflow YAML」。本项目的现有契约使该方案不成立，理由是实测确认的三条硬约束：

**（1）`target` 是会话内临时引用，不可持久化。** 三端 driver 的写动作 `input_schema` 都要求
`target: {snapshot_id, revision, node_id}`。实测 `windows_uia_driver._record()` 的实现表明 driver 只保留**一个** `self._current` 快照，`snapshot_id` 或 `revision` 不匹配即抛
`DRIVER.STALE_SNAPSHOT`。因此录制期得到的 `node_id` 在下一次运行必然失效——录制产物**必须**存 locator（描述性条件），而不是 target（引用）。

**（2）driver 对 locator 歧义 fail-closed，且没有序号字段。** `LOCATOR_SCHEMA` 的字段集为
`role/name/value/automation_id/class_name/framework_id/states/actions/match`，`match` 只能是
`{"const": "exact"}`；`_resolve()` 在候选数 > 1 时抛 `DRIVER.AMBIGUOUS`。这意味着：录制器不能靠「第 N 个匹配项」表达目标，**必须**产出在目标快照中**唯一**的 locator，否则回放必然失败。

**（3）三端能力不对称，录制产物不能假装平台无关。** 实测比对三个 driver：

| | 共有 | 平台独有 |
| --- | --- | --- |
| action | `find` `focus` `invoke` `pointer_click` `snapshot` `type_text` | win `list_windows`；mac `list_apps`；linux `list_applications` `set_text` `expand` `collapse` `toggle` `inspect_session` |
| locator 字段 | `role` `name` `value` `states` `actions` `match` | win `automation_id` `class_name` `framework_id`；mac `subrole` `identifier` `description`；linux `bus_name` `object_path` `toolkit_name` `attributes` `description` |

注意「同名不同义」：写入文本值在 win/mac 是 `set_value`，在 linux 是 `set_text`。role 词表同样不统一——win 通过 `CONTROL_TYPE_ROLES` 映射 ControlTypeId，mac 直接透传 AX role，linux 用 `_normalize_role` 归一化 AT-SPI 名称。

结论：录制产物是**独立的、平台标注的中间制品**。它记录「在某平台上观察到了什么」，编译期再降级为 Workflow。录制产物绑定其录制平台（§6）；跨平台指录制器本身三端可运行，**不指一份录制能三端回放**。

## 2. 编码与版本

- `apiVersion` 固定 `ai-auto-desktop.dev/v1alpha1`，`kind` 固定 `Recording`。
- JSON 是规范语义来源，应按 RFC 8785 生成 canonical bytes 后计算摘要；YAML 仅作 authoring/审阅格式，加载后必须产生相同 JSON 数据模型。
- 未知核心字段必须 fail-closed。开放边界仅限 `extensions`、`observed.provenance`、`steps[].logic.inputs` 与结构化错误 `details`。
- 录制产物**不得**被直接执行。执行前必须经过 §7 编译为 Workflow Descriptor，并接受与手写 workflow 完全相同的 schema、policy、risk、permission 检查。录制来源**不构成**任何信任提升。

## 3. 顶层对象

| 字段 | 必需 | 语义 |
| --- | --- | --- |
| `apiVersion` / `kind` | 是 | 格式标识。 |
| `metadata` | 是 | `name` 必需；可含 `version` `description` `labels` `annotations`。 |
| `capture` | 是 | 录制环境事实，见 §4。**必须**含 `window`：回放需要一个精确窗口选择器，实测 driver 对空选择器返回 `DRIVER.INVALID_REQUEST`（`window must contain an exact selector`）。 |
| `steps` | 是 | 有序录制步骤列表，**不得为空**。实测 `workflow.schema.json` 将顶层 `steps` 定义为 `nonEmptySteps`（`minItems: 1`），空 step 列表会被直接拒绝（`$.steps: [] should be non-empty`）。因此空录制**不可编译**，必须报 `RECORDING.EMPTY` 错误而非警告；全部步骤被 `enabled: false` 禁用时同理。 |
| `inputs` | 否 | 外部输入声明，直接映射为 workflow `inputs`。`logic` 条件与 `text.source: input` 引用的输入**必须**在此声明或由 `type_text` 自动推导。实测现有 workflow 编译器**不校验**输入引用：删除已被 `${{ inputs.X }}` 引用的声明，甚至引用完全不存在的输入，descriptor 仍被接受。因此录制编译器**必须**自行校验，缺失时报 `RECORDING.ORDER_INVALID`。 |
| `redaction` | 是 | 脱敏策略与已脱敏字段清单，见 §5。 |
| `platform_binding` | 是 | 录制平台绑定，见 §6。 |
| `extensions` | 否 | `domain/key` 命名扩展。 |

## 4. `capture`：录制环境事实

```json
{
  "platform": "windows",
  "recorded_at": "2026-08-29T10:00:00Z",
  "driver": {"name": "desktop.windows_uia", "version": "0.1.0", "manifest_digest": "sha256:..."},
  "probe": {
    "windows.session": "available",
    "windows.input_desktop": "available",
    "windows.integrity": "available",
    "windows.dpi": "degraded"
  },
  "environment": {
    "integrity_level": "medium",
    "pointer_quantisation": 2.5,
    "scaled_display": true
  }
}
```

`platform` 是 `windows|macos|linux` 之一。`driver.manifest_digest` 固定录制时的 action 契约，编译期必须校验；digest 不匹配时必须报 `RECORDING.DRIVER_DRIFT` 而非静默继续。

`probe` 与 `environment` **必须**由 `probe_capabilities()` 的真实输出填充，不得手工编造。它们的作用是让「回放失败」可归因：

- `windows.integrity` 为 `medium` 而目标应用被提权 → UIPI 会阻断输入，这不是 locator 问题。
- `windows.dpi` 为 `degraded` 且 `pointer_quantisation` > 1 → 指针坐标量化到该步长；录制时命中的小目标在回放时可能落在相邻像素。实测本机为 2.5，即 1 物理像素的目标无法精确寻址。
- `windows.session` 为 `unavailable`（Session 0）→ 根本没有交互桌面，录制本身就不应开始。

`environment` **不得**包含用户名、主机名、路径、窗口标题或任何环境标识值；只允许影响可回放性的派生事实。

## 5. `redaction`：只保护凭据，不牺牲可判断性

> **2026-09-03 修订。** 本节此前要求「录制器必须默认丢弃节点 `value`，只保留
> `observed.had_value` 布尔」。该要求已被撤销：它既牺牲了必要信息，也建立在一个
> 关于平台行为的错误前提上。修订依据是对真实 Win32 控件的实测，见 §5.1。
>
> 撤销的具体条目：默认 `value_policy: drop`、默认 `title_policy: drop`、
> 以及「保留字面值须逐条登记进 `disclosed`」。保留的条目：密码类元素的值不得进入
> 产物且不可解除（§5.2），`type_text` 的凭据必须外提为 input（§7.3）。

录制天然会碰到敏感数据，但「敏感」不等于「所有值」。丢弃普通字段的值会直接破坏这套
工具的用途：判断表单是否填对、检索词是否正确、状态是否符合预期，都依赖读到那个值。
一份读不到值的录制，操作者无法复核，AI 也无法判断该不该继续。

### 5.1 实测：平台已经划好了这条界

用一个真实窗口测量，其中一个普通 `TextBox`、一个 `UseSystemPasswordChar = true` 的
密码框，两者相邻：

| 控件 | UIA `ValuePattern.CurrentValue` | `IsPassword` |
| --- | --- | --- |
| 普通输入框 | `"quarterly-report-2026"` | `false` |
| 密码框 | **无值（`null`）** | `true` |

结论有两点，都与原规范的假设相反：

1. **密码值本来就读不出来。** 不是录制器泄露了它，而是平台在 `ValuePattern` 层面
   就不提供。原规范花力气防的这件事，操作系统已经做了。
2. **普通值读得出来，且必须保留。** 原规范用「Notepad 的 `document` 节点 `value`
   含全部正文」论证要一律丢弃——但那说明的是**该节点**信息量大，不是所有 `value`
   都敏感。按控件类型区分即可，无须全面丢弃。

因此规范改为：**默认保留 `value`；由 `states.protected` 单独标记不可读的元素。**

**写入不受影响，这是必须保住的能力。** 同一实测确认：`set_value` 能成功写入密码框
（以窗口标题回显 `len` 与字符校验和验证，写入 19 字符、校验和吻合），随后仍无法读回。
即「可写不可读」，正是自动登录、自动填充凭据需要的语义。所以：

- 录制器与运行时**不得**因 `protected` 为 true 而拒绝写动作。
- **不需要**为写入密码设计任何「解除」「授权」「例外登记」机制——读取本就不可能，
  写入本就允许，再加一道门只会阻断正常用途。
- 实测补记：`type_text` 走 `SendInput`，在装有输入过滤软件的机器上可能被拦
  （本机即如此，见 README「已知环境限制」）；填凭据**应当**优先用 `set_value`。

### 5.2 `states.protected`：标记，而不是过滤器

driver **必须**为每个元素报告 `states.protected`（Windows 取
`IUIAutomationElement::CurrentIsPassword`）。它的作用是**说明**，不是删除：

- 它把「这里没有值」与「这里的值不可读」区分开。缺少该标记时，密码框看起来与一个
  读取失败的字段完全一样，会让调用方去排查一个不存在的故障。
- 元素描述（`summary` / outline）**必须**同时以结构化字段与可读标记呈现它，使调用方
  无须解析散文即可判断。
- locator **必须**支持按 `states.protected` 匹配。密码框常常没有稳定的 name，
  「本表单里那个密码框」需要可表达。

任何 `states.protected` 为 true 的元素，其值**不得**进入录制产物；该项**不可**解除。
这是本节唯一保留的硬性禁止——而它成本极低，因为平台本就不提供该值。

### 5.3 `redaction` 对象

```json
{
  "screenshots": "none",
  "disclosed": []
}
```

- `screenshots` **必须**显式声明 `none|bounds_only|full`，默认 `none`。选择 `full`
  时必须登记理由：截图会连带捕获画面上的任何内容，**这是唯一仍需谨慎的默认项**——
  它绕过了语义树，因此绕过了 §5.1 的平台保护。
- `value_policy` 与 `title_policy` **已移除**。默认保留值与标题。窗口标题是定位窗口
  的主要依据之一（§11.4），默认丢弃会让录制无法回放。
- `disclosed` 保留但语义收窄：不再用于登记「我要保留普通字面值」（那是默认行为），
  仅用于登记 `screenshots: full` 之类确有风险的显式选择。

### 5.4 录制产物与编译产物必须同时清理

实测教训（本项目自身）：先只修编译产物，`.workflow.json` 已不含凭据，但
`.recording.json` 仍把它逐字写在 `argument` 字段里——而两个文件在同一目录。
从一个文件里取走凭据、又写进它旁边的另一个文件，只是让泄露看起来被处理过。

因此：凭据的外提**必须**同时作用于**可再编辑的录制产物**与**编译出的 workflow**。
录制产物中该步骤的字面量字段必须缺省，`protected` 标记必须随产物持久化——否则
重新打开一份录制后，编译器会因为不知道该字段是凭据而重新内联它。

### 5.5 捕获机制约束

录制器**不得**安装全局输入钩子（`SetWindowsHookExW`/`WH_KEYBOARD_LL`、`CGEventTap`、X11 `XRecord`）。这类机制会捕获系统内所有按键，包括在与录制目标无关的应用中输入的凭据，等同于系统级键盘记录器。

录制**必须**仅依赖可访问性事件（UIA `AddFocusChangedEventHandler`/`AddPropertyChangedEventHandler`/`AddStructureChangedEventHandler`、AX `AXObserver`、AT-SPI listener）。实测确认这足以合成 locator：未安装任何键盘钩子的情况下，UIA focus 事件已提供完整元素身份（`role`、`name`、`class_name`、`automation_id`、`process_id`）。

由此产生的能力边界必须诚实声明：逐键还原按键序列（哪个键、何顺序、何时序）是不可能的，这是放弃钩子的必然代价。元素**值的变化是可观察的**——实测在输入确实送达控件时会收到 `UIA_Text_TextChangedEventId`(20015) 与 `UIA_ValueValuePropertyId`(30045) 属性变更。普通字段的新值按 §5.1 予以保留；密码框不会产生可读的新值（§5.1 实测）。

**事件投递方式取决于 COM 套间，实现必须显式声明并匹配。** UIA 事件经 COM 回调投递，而回调如何到达取决于订阅线程所在套间，实测两者行为不同：

| 套间 | 是否需要消息泵 | 回调所在线程 |
| --- | --- | --- |
| STA（`COINIT_APARTMENTTHREADED`） | **需要**。只 `sleep` 不 dispatch 则回调永不到达 | 订阅线程本身 |
| MTA（`COINIT_MULTITHREADED`） | **不需要**。仅 `sleep` 也能收到 | COM 的 RPC 工作线程 |

UIA 要求 MTA（Rust 实现实测：在窗口框架所需的 STA 上构造 driver 会得到
`RPC_E_CHANGED_MODE`，故 driver 运行在独立的 MTA 线程上）。因此捕获实现**不得**依赖消息泵，而**必须**假定回调在任意 RPC 线程上并发到达：所有跨线程状态**必须**加锁，事件缓冲区**必须**是线程安全的。

若某平台实现选择 STA，则**必须**在该线程内运行消息泵，且**不得**以 `sleep` 等待事件——实测缺少消息泵会产生「全部交互 0 事件」的假象，与「用户未操作」在观测上完全无法区分，属于高危假阴性。

**已知盲区必须显式提示，不得静默丢弃。** 实测两类交互不产生任何可访问性事件：（a）点击不可聚焦元素（如原生 `STATIC` 标签）——该元素本身不响应点击；（b）纯 hover 与鼠标移动。命中盲区时录制 UI **必须**告知操作者该位置无可记录交互，**不得**静默忽略，也**不得**退化为坐标点击（§7.1 已禁止坐标进入录制）。

编译期必须校验：`disclosed` 中出现的字段确实存在，且未触碰 §5.2 的不可解除类别；否则 `RECORDING.REDACTION_INVALID`。

## 6. `platform_binding`：录制产物绑定单一平台

**录制产物只能在其录制平台上回放。** 跨平台指的是**录制器本身**能在 Windows / macOS / Linux 上运行，而不是一份录制产物能在三端回放。

这个定位使规范大幅简化：不需要 tier 分级，不需要 per-platform 覆盖，不需要跨平台 locator 等价推断。

```json
{
  "platform": "windows",
  "replay_platforms": ["windows"]
}
```

规则：

- `platform` **必须**等于 `capture.platform`。
- `replay_platforms` **必须**是恰好一个元素，且等于 `platform`。保留数组形式仅为向前兼容，当前不得包含多个值。
- 编译时若目标平台不等于 `platform`，**必须**报 `RECORDING.PLATFORM_MISMATCH` 并拒绝，**不得**尝试推断等价 locator、映射 role 词表或替换同义 action。
- 编译产物的 `requires.platforms` **必须**设为 `[platform]`，由运行时再做一次平台校验。

这条边界不是保守，而是三端能力实测不对称的直接结论（§1 约束 3）：locator 字段集三端各异（win 有 `automation_id`/`class_name`/`framework_id`，mac 有 `subrole`/`identifier`，linux 有 `bus_name`/`object_path`）；写值动作在 win/mac 是 `set_value`、在 linux 是 `set_text`；role 词表由各 driver 独立生成（win 映射 ControlTypeId、mac 透传 AX role、linux 归一化 AT-SPI 名称）。在这些差异之上做自动跨平台转换，只会产出**看起来能跑、实际不可靠**的产物。

录制器的跨平台性另行表达：`capture.driver` 与 `capture.probe` 记录录制平台的真实能力，UI 与流水线在三端共用同一套实现（见 `docs/architecture/record-replay.md` §9 的分期）。

## 7. 步骤模型

每个步骤是三类之一：`interaction`、`assertion`、`logic`。前两类由录制产生，第三类由人工插入（这正是「添加自定义逻辑」的落点）。

所有步骤共有：唯一 `id`、`kind`、可选 `description`、可选 `enabled`（默认 true，false 表示保留在产物中但不编译，用于无损禁用而非删除）、可选 `extensions`。

### 7.1 `interaction`

```json
{
  "id": "click_save",
  "kind": "interaction",
  "action": "invoke",
  "window": {"role": "window", "name": "notes.txt - Notepad", "class_name": "Notepad"},
  "locator": {"role": "button", "name": "保存", "class_name": ""},
  "disambiguation": {"strategy": "unique", "verified": true},
  "observed": {
    "role": "button",
    "value": null,
    "actions": ["pointer_click", "invoke"],
    "states": {"enabled": true, "offscreen": false, "focusable": false, "protected": false},
    "bounds": {"x": 1113, "y": 176, "width": 47, "height": 28},
    "ancestry": [{"role": "window", "class_name": "Notepad"}],
    "sibling_ordinal": 0,
    "provenance": {"framework_id": "Win32"}
  }
}
```

`action` 必须是录制平台 driver 实际声明的 action。`locator` 必须只使用该平台 `LOCATOR_SCHEMA` 允许的字段。

`observed` 是**证据**而非回放输入：它记录录制瞬间的事实，用于 UI 展示、locator 修复建议、以及回放失败时的差异归因。`observed.bounds` **不得**用于生成坐标点击；现有契约要求 `pointer_click` 的坐标必须由回放时的 fresh 语义节点 bounds 推导，录制期 bounds 只是诊断信息。

### 7.2 `disambiguation`：唯一性是录制器的责任

因为 driver 对歧义 fail-closed，录制器**必须**在落盘前证明 locator 在当时快照中唯一。实测数据（Notepad，46 节点，两次独立启动）：

| locator 字段 | 唯一率 | 跨会话稳定率 |
| --- | --- | --- |
| `role` | 10.9% | 100% |
| `role+name` | 82.6% | 100% |
| `role+automation_id` | 43.5% | 100% |
| `role+class_name` | 19.6% | 100% |
| `role+name+class_name` | **87.0%** | **100%** |
| `role+name+automation_id+class_name+framework_id` | 87.0% | 100% |

两点结论直接写进规范：

1. 录制器**应当**按 `role` → `+name` → `+class_name` → `+automation_id` → `+framework_id` 的顺序逐步收窄，**并在达到唯一后停止**。继续添加字段不提升唯一率（87.0% → 87.0%），却降低对 UI 变更的容忍度。
2. 仅靠字段无法达到唯一时（实测约 13% 的节点），必须走 `strategy` 降级链，且每一级都要在产物中显式记录：

- `unique`：字段组合已唯一。首选。
- `scoped`：把搜索范围收窄到某个祖先容器后唯一。产物必须记录该容器的 locator。实测警告：祖先并非总能消歧——`["pane","","Microsoft.UI.Content.DesktopChildSiteBridge"]` 的 3 个节点**共享同一父节点**，此路不通；而 `["pane","","InputSiteWindowClass"]` 的 3 个节点父节点各不相同，此路可行。录制器必须实际验证，不得假定。
- `ordinal`：在同构候选组内取第 N 个。**这是最后手段**，因为现有 driver locator 无序号字段，编译器必须把它降级为「`find` 取回候选后由 workflow 层判定」的显式形式，或直接拒绝。产物必须标记 `fragile: true`。
- `unresolved`：录制器无法证明唯一。该步骤**必须** `enabled: false` 落盘并要求人工处理，**不得**乐观编译。

`verified: true` 表示录制器实际执行过一次解析并确认候选数为 1。未验证的 locator 必须 `verified: false`，编译期视为警告。

### 7.3 `type_text` 与敏感输入

**2026-09-03 修订。** 此前要求「录制到的文本默认必须外提为 workflow input，录制器不得默认产出 `literal`」。现按 §5 收窄：

- 普通字段录制到的文本**应当**保留为 `literal`。它通常就是录制的目的，外提成 input 会让一份录制无法直接回放，也无法复核录到了什么。
- **`states.protected` 为 true 的字段，其文本必须外提**，且必须标 `sensitive: true`、`required: true`。凭据不得进入产物（§5.2、§5.4）。

外提后的形态：

```json
{
  "id": "enter_name",
  "kind": "interaction",
  "action": "type_text",
  "locator": {"role": "document", "name": "文本编辑器"},
  "text": {"source": "input", "input": "name", "sensitive": true},
  "disambiguation": {"strategy": "unique", "verified": true}
}
```

`text.source` 为 `input` 时编译为 `${{ inputs.<name> }}`；为 `literal` 时直接内联。`literal` **不再**需要登记进 `redaction.disclosed`——普通字面量是默认且预期的形态。

输入名**应当**由步骤 id 派生（实现取 `<step_id>_secret`），使同一份录制里的两个凭据不会共用一个 input：登录密码与确认密码共用会静默填入同一个值。

### 7.4 `assertion`：回放判定的唯一合法形式

录制器观察到的状态变化必须编译为 postcondition，而不是隐式的「等一会儿看看」。

```json
{
  "id": "save_completed",
  "kind": "assertion",
  "of_step": "click_save",
  "observe": {"action": "find", "locator": {"role": "window", "name": "另存为"}},
  "expect": {"mode": "exists"},
  "timeout": "5s",
  "poll_interval": "200ms"
}
```

`expect.mode` 为 `exists|absent|value_equals|value_matches|state_equals`。编译时映射为宿主 action 的 `postcondition.observe` + `condition`。现有契约要求观察动作必须是 `read_only` 且全部错误为 `not_applied`——`find` 与 `snapshot` 满足，因此 `observe.action` **必须**限定在这两者之内。

**`absent` 需要 `expect: "optional"`**（2026-09-03 实测后补）。`find` 默认在零匹配时报
`DRIVER.NOT_FOUND`，而该错误是 `retryable`，会被轮询当成「还没到」；于是 `absent` 断言必然
耗尽 timeout 再失败——**永远无法满足**。`expect: "optional"` 让零匹配成为普通结果
（`found: false`），`absent` 才写得出来：

```yaml
condition: ${{ not observation.found }}
observe:
  uses: desktop.windows_uia.find@1
  with:
    window: { title: "..." }
    locator: { name: "..." }
    expect: optional          # 少了这个，断言永远不成立
```

`found` 在两条路径上都有，所以同一个条件写法在"找到了"和"没找到"两种结果下都成立。
报错仍是默认行为：为了操作而取元素时，清楚的 `DRIVER.NOT_FOUND` 好过一个空结果在后面
以"target 不见了"的形式炸开。

`optional` **不放宽歧义**——多个匹配依然报 `DRIVER.AMBIGUOUS_MATCH`。「在不在」和
「是哪一个」是两个问题，把前者答成"给你其中一个"正是点错按钮的由来。

**`value_matches` 是子串包含，不是正则**（2026-09-03 实测后补）。表达式求值器明确拒绝函数与
方法调用（`expression.rs`：`function and method calls are not allowed`），所以没有任何 matcher
可以调用——实测 `value.matches("h.*o")` 和 `value.startswith("he")` 连 `validate` 都过不了。
语法里唯一的包含运算是 `in`，因此该模式编译为 `${{ "子串" in observation.node.value }}`。
文档和 UI 都不得把它写成正则：第一个稍复杂的模式就会暴露。

五种模式各自读取的字段（均已在真机观察里确认存在）：

| mode | 条件 |
|---|---|
| `exists` | `${{ observation.found }}` |
| `absent` | `${{ not observation.found }}`，且 observe 必须带 `expect: optional` |
| `value_equals` | `${{ observation.node.value == "…" }}` |
| `value_matches` | `${{ "…" in observation.node.value }}` |
| `state_equals` | `${{ observation.node.states.<flag> == True/False }}` |

`state_equals` 的 flag 只能取 `enabled/offscreen/focusable/focused/read_only/protected`——
观察里只有这六个。**引用一个不存在的字段不是「条件为假」，而是整个运行以
`EXPRESSION.EVALUATION_FAILED` 失败**（实测），所以录制侧必须在编译前拦住拼错的 flag。

回放判定语义：**没有 assertion 的 interaction 步骤不构成「回放成功」**。它只证明动作被派发，不证明产生了预期效果。录制器应当为每个写动作自动提议一条 assertion，并在 UI 中标出缺失 assertion 的步骤。

### 7.5 `logic`：人工插入的自定义逻辑

`logic` 步骤不由录制产生，它是编排能力的载体，直接映射到已有 workflow step 类型，因此**不引入新的执行语义**：

| `logic.type` | 编译目标 | 说明 |
| --- | --- | --- |
| `condition` | `if` / `switch` | `when` 为完整 `${{ }}` 表达式 |
| `loop` | `foreach` / `while` | 必须提供 `max_items`/`max_iterations`，与现有有界要求一致 |
| `assign` | `set` | 只能写 `mutable: true` 变量 |
| `group` | `block` | 建立独立 handler/finally 作用域 |
| `fail` / `return` | 同名 step | |
| `script` | `script` | 纯计算，能力边界见 §7.6 |

`logic` 步骤可以包含子步骤列表，从而形成嵌套。子列表与顶层列表一样各自构成独立 DAG scope——这与 workflow 规范 §4 的 scope 规则一致，编译时 `depends_on` 的规范化（省略即依赖前一 sibling）由现有编译器负责，录制格式不重复定义。

**边界声明**：`logic` 只是现有 step 类型的 authoring 视图。任何在 workflow 层不可表达的语义都不得通过 `logic` 引入；遇到无法映射的构造必须报 `RECORDING.LOGIC_UNSUPPORTED`。

### 7.6 `script` 的能力边界

`logic.type: script` 编译为 workflow 的 `script` step，继承其全部既有限制。因为这些限制决定了它能否承担某类自定义逻辑，此处明确列出（详见 `docs/architecture/script-execution.md`）：

- **只能是 Python，且只有一个文件。** schema 的 `runtime` 是 `{"const": "python"}`，不是枚举；来源二选一（`source` 内联，上限 1 MiB；或 `entrypoint` 文件路径），由 `oneOf` 强制互斥。
- **输入经 stdin 传入一个 JSON 值**，不是命令行参数也不是环境变量。`inputs` 中的 `${{ }}` 在宿主侧求值后传入；序列化使用 `allow_nan=False`，故 `NaN`/`Infinity` 会失败而非静默通过。
- **stdout 必须是恰好一个 UTF-8 JSON 值**，且必须通过必填的 `output_schema` 校验；否则 `SCRIPT.OUTPUT_INVALID`。
- **纯计算，无外部 IO。** sandbox 的 `network`/`filesystem`/`environment` 三个边界在 schema 中都是 `{"mode": {"const": "deny"}}`——deny 是唯一可表达的值；`capabilities` 在 `scriptStep.properties` 中不存在且 `unevaluatedProperties: false`。唯一可调参数是 `max_output_bytes`。
- **不得用于 UI 交互。** 脚本无法调用桌面 driver。所有 UI 动作必须是 `interaction` 步骤，以保证 policy / risk / confirmation 检查不被绕过。
- **两道独立的门。** 需要 CLI `--allow-scripts`（否则 `SCRIPT.SANDBOX_DENIED`），**且**当前平台有可用沙箱（否则 `SCRIPT.SANDBOX_UNAVAILABLE`）。Linux 为 `available`；Windows 为 `degraded`——资源上限、进程树回收、空环境与隔离解释器均由内核强制，但**网络与文件系统未隔离**；macOS 仍 fail-closed。UI **必须**展示当前平台的沙箱状态与未强制边界，**不得**让操作者以为脚本在任何平台上都被完全隔离。

因此录制 UI **应当**默认提供 `condition`/`loop`/`assign`/`group`（无需沙箱、三端行为一致），把 `script` 作为进阶选项，并明示上述边界；**不得**让操作者以为脚本可以联网或读写文件。

## 8. 编译与重排

编译方向是**单向**的：`Recording → Workflow Descriptor`。不定义反向编译，因为 workflow 允许的表达远超录制可还原的范围，双向同步会产生无法收敛的语义歧义。录制产物是可长期保存、可再编辑的源；workflow 是派生产物。

重排语义：`steps` 的**列表顺序**是唯一的顺序真相。重排就是改变列表顺序，编译器按现有规则把「未声明依赖」规范化为串行链。因此：

- 重排后编译**必须**重新校验数据依赖。实测确认现有 `compile_descriptor()` 已覆盖这一项：把引用 `steps.X.output` 的步骤移到 X 之前，编译期即报 `uncovered_step_reference`。录制编译器复用该校验，不自建。
- **引用完整性必须由录制编译器自行校验。** 实测发现现有 workflow 编译器**不覆盖**此项：删除被引用的步骤后，descriptor 仍被接受（因为现有语义是「引用必须被 `depends_on` 覆盖」，而缺失的 step 不产生依赖）；引用完全不存在的 id 同样被接受。**输入引用同样不被覆盖**——以已知可编译的 descriptor 为基线做单点改动，删掉 `should_save` 的声明而保留 `${{ inputs.should_save }}` 引用，仍被接受；引用凭空捏造的输入亦然。因此录制编译器**必须**在生成 workflow 之前校验：每个 `steps.<id>` 引用与每个 `assertion.of_step` 都指向存在且 `enabled: true` 的步骤，且每个 `${{ inputs.X }}` 都有对应声明，否则报 `RECORDING.ORDER_INVALID`。
- 删除步骤时，任何引用它的 assertion 或表达式必须报错。UI **应当**默认提供 `enabled: false` 而非删除，因为禁用不产生悬空引用。
- 录制顺序会被保留在 `metadata.annotations` 的 `ai-auto-desktop.dev/recorded-order` 中，供 UI 显示「已偏离录制顺序」，但它**不参与**执行语义。

### 8.1 编译展开

一条 `interaction` **必须**展开为三个 workflow step：`snapshot` → `find`（locator → target）→ 真实动作（消费 target）。这是 §1 约束（1）的直接后果：动作需要会话内有效的 target，而产物中只有 locator。

一条 `assertion` **不得**编译为独立步骤，而**必须**附加为 `of_step` 所指步骤的 `postcondition`，其 `observe` 使用 `find` 或 `snapshot`。

该展开已实测验证：含三种步骤类型的录制编译后通过未修改的 `workflow.schema.json`，并被 `ai_auto_desktop.compiler.compile_descriptor()` 接受，嵌套 scope 内依赖亦正确规范化。

由此产生的预算影响必须显式处理：一条用户动作消耗 3 个 executed step，编译器**应当**据此设定 `budgets.max_executed_steps` 默认值并在 UI 中展示占用。

## 9. 结构化错误

录制与编译阶段保留以下 code（与 workflow 运行时 code 命名空间分离）：

`RECORDING.INVALID`, `RECORDING.VERSION_UNSUPPORTED`, `RECORDING.EMPTY`,
`RECORDING.DRIVER_DRIFT`, `RECORDING.REDACTION_INVALID`, `RECORDING.PLATFORM_MISMATCH`,
`RECORDING.LOCATOR_UNRESOLVED`, `RECORDING.LOCATOR_FRAGILE`,
`RECORDING.ORDER_INVALID`, `RECORDING.LOGIC_UNSUPPORTED`,
`RECORDING.ASSERTION_MISSING`（警告级）。

## 10. 与现有规范的关系

- 本格式**不修改** `workflow-descriptor-v1alpha1`，不新增 workflow step 类型，不放宽 locator/target 语义。
- 编译产物必须通过 `schemas/workflow/v1alpha1/workflow.schema.json`。
- 录制不绕过 policy：编译后的 workflow 仍须声明 `budgets`，并接受 risk/permission/confirmation 检查。录制时操作者「手动点过一次」**不构成**运行时确认。
- 桌面动作的 `observe → resolve → precondition → policy/confirm → execute → re-observe → postcondition` 闭环由运行时保证；录制格式只负责产出正确的 locator 与 postcondition，不参与该闭环。
## 11. 回放前置条件（实测补充）

以下三项由真实回放暴露，规范此前均未覆盖。共同点：违反任一项都能通过
`validate`，但一定无法 `run`。

### 11.1 `requires.permissions` 必须声明

编译产物须在 `requires.permissions` 声明所需权限，否则运行期被策略拒绝
（`POLICY.DENIED`，`undeclared_permissions`）。`validate` **不执行策略检查**，
因此缺失不可见。权限按启用步骤的动作取并集：读取类动作只需 `desktop.observe`，
驱动 UI 的动作追加 `desktop.input`。为只读录制声明 input 属过度授权。

### 11.2 动作必须有权限分类

未登记的动作不得编译。实测未知动作会被展开成不存在的 driver 动作且只声明 observe，
未来新增写动作若漏登记将静默少声明权限。缺分类时报 `RECORDING.LOGIC_UNSUPPORTED`。

### 11.3 `disambiguation.strategy` 不得未经解析即声称唯一

转换器只有事件流、没有快照，**无法**判断 locator 是否唯一，必须写
`strategy: unresolved`。只有实际解析过（`verify_locators`）且恰好 1 个匹配，
才可升级为 `unique / verified: true`；0 或多个匹配必须保持 `unresolved`，
且**已 verified 的步骤在重新校验后必须能被降级**（录制可编辑，界面会变）。
编译器拒绝任何 `unresolved` 步骤，报 `RECORDING.LOCATOR_UNRESOLVED` 并携带步骤 `id`。

### 11.4 窗口选择器

`capture.window` 为必需；步骤级 `window` 是**可选覆盖**，仅用于跨窗口流程。
两者皆缺时报 `RECORDING.WINDOW_UNRESOLVED` 并指名步骤。
