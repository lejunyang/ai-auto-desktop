# 工作流描述文件 v1alpha1

状态：Alpha，2026-08-24。本文中的“必须”“不得”“应当”具有规范含义。规范 JSON Schema 位于 `schemas/workflow/v1alpha1/workflow.schema.json`，Capability Manifest Schema 位于 `schemas/capabilities/v1alpha1/capability-manifest.schema.json`。

## 1. 目标与边界

Workflow Descriptor 描述可移植的意图、控制流、预算、权限与失败语义，不暴露 UIA/AX/AT-SPI 句柄或进程 IPC 细节。JSON 是规范语义和 canonical hash 的来源；YAML 只是 authoring 格式，加载后必须转换为相同 JSON 数据模型。

执行器必须先验证并编译 descriptor，再运行不可变计划。未知核心字段必须 fail-closed；只有 `extensions`、action `with`、script `inputs`、JSON Schema 片段及结构化错误 `details` 是明确开放边界。

OCR 必须是显式 `action`，例如 `fixture.ocr@1` 或当前 Tesseract provider 的 `vision.ocr.recognize@1`。运行时不得在 locator 失败、语义树为空或 action 失败时偷偷截取全屏并执行 OCR。是否捕获图像、裁剪范围、识别语言和置信度门槛都必须由 workflow/capability contract 显式表达并接受策略检查。

## 2. 编码、版本与 YAML 规则

- `apiVersion` 固定为 `ai-auto-desktop.dev/v1alpha1`；破坏核心语义的变化必须使用新 API version。
- `kind` 固定为 `Workflow`。
- `metadata.version` 是工作流自身的 SemVer，不决定解析语义。
- `uses: fixture.ocr@1` 中 `fixture` 必须精确匹配 manifest 的 `metadata.name`，`ocr` 是 action map key，`@1` 必须等于 `contract_major`。
- `requires.capabilities[].version` 是 provider 版本约束；编译后的计划应固定具体 provider 版本和 schema digest。
- JSON 应按 RFC 8785 生成 canonical bytes 后计算摘要。
- YAML loader 必须拒绝重复 key、merge key、alias、custom tag、非字符串 map key，并限制深度、节点数和标量长度。YAML 标量类型不得改变 JSON 等价值。

## 3. 顶层对象

| 字段 | 必需 | 语义 |
| --- | --- | --- |
| `apiVersion` / `kind` | 是 | 格式标识。 |
| `metadata` | 是 | `name` 必需；可含 `version`、`description`、`labels`、`annotations`。 |
| `requires` | 否 | runtime、平台、capability 版本和权限前置条件。缺少非 optional capability 时编译失败。 |
| `inputs` | 否 | 名称到 `{schema, required=false, default?, sensitive=false}` 的映射。`required: true` 与 `default` 不得并存。 |
| `variables` | 否 | 名称到 `{schema, mutable=false, initial}` 的映射；`initial` 可为 JSON literal 或表达式。只有 `mutable: true` 的变量可被 `set` 修改。 |
| `outputs` | 否 | 名称到 `{value, schema?, sensitive=false}` 的映射；workflow 成功时求值并校验。 |
| `defaults` | 否 | 未在 step 指定时采用的 `timeout` 和 `retry`。 |
| `budgets` | 是 | 必须含 `max_duration` 和 `max_executed_steps`；可含取消后的 `cleanup_timeout`。 |
| `policy` | 否 | 风险上限、确认、污点输入、截图和 desktop writer 策略。descriptor 只能收紧宿主策略。 |
| `steps` | 是 | 非空、按顺序执行的 step 列表。 |
| `on_error` | 否 | workflow 作用域错误处理器。 |
| `finally` | 否 | 无论成功、失败、取消或超时均执行的清理步骤。 |
| `extensions` | 否 | 以 `domain/key` 命名的扩展；必需扩展不受支持时必须失败。 |

Duration 是不含空格的正整数加 `ms`、`s`、`m` 或 `h`。所有 timeout 都转换为基于 monotonic clock 的绝对 deadline；子作用域有效 deadline 是自身 deadline 与所有父级剩余 deadline 的最小值。

表达式使用完整字符串 `${{ ... }}`。它是只读、确定性、无副作用 DSL，只能访问 `inputs`、`vars`、`steps`、当前控制流绑定和错误处理器绑定；不得访问文件、网络、环境变量、时钟、随机源或 secret。字符串插值不等于表达式，尤其不得把表达式拼进 script source。

## 4. 通用步骤字段

每个 step 都必须有作用域内唯一的 `id` 和 `type`，并可带：

- `description`：人类说明。
- `if`：布尔表达式；false 时状态为 `SKIPPED`，不执行 retry/handler，但仍执行该 step 的 `finally`。
- `timeout`：整个 step 的墙钟预算，包含所有 attempts、backoff、嵌套步骤和 postcondition。
- `attempt_timeout`：单次 attempt 预算。
- `retry`：结构化重试策略。
- `on_error`、`finally`：step 作用域处理器与清理步骤。
- `extensions`：命名扩展。

step 路径由嵌套 `id` 构成；`foreach` 运行记录还必须带 index。全局 `max_executed_steps` 计算实际进入执行状态的 step attempt，不能通过嵌套控制流绕开。

## 5. 步骤类型

### 5.1 `action`

```yaml
- id: read_text
  type: action
  uses: fixture.ocr@1
  with:
    text: Retry
    language: en
  effect:
    class: read_only
  risk:
    category: observe
    level: low
  postcondition:
    condition: "${{ steps.read_text.output.confidence >= 0 }}"
    timeout: 2s
    poll_interval: 100ms
```

`with` 必须通过已解析 action 的 `input_schema`，输出必须通过 `output_schema`。`effect.class` 是 `read_only`、`idempotent`、`non_idempotent` 或 `contextual`。`risk` 必须是对象：`category` 为 `observe|navigate|input|modify|send|delete|purchase|authorize|install|execute_script|capture_screen|custom`，`level` 为 `low|medium|high|critical|contextual`。

Manifest 给出默认 effect/risk；descriptor、driver 动态判断和宿主策略都只能提高，不能降低有效等级。`precondition` 在执行前求值；`postcondition` 可在限定 timeout 内轮询。桌面动作必须完成 `observe → resolve → precondition → policy/confirm → execute → re-observe → postcondition` 闭环；每次 attempt 必须重新解析 locator，不得跨 snapshot 使用 `node_id`。

### 5.2 `script`

`runtime` 为 `python|javascript|shell`；`source` 和 `entrypoint` 必须且只能提供一个。`inputs` 是唯一的数据输入，stdout 结果必须通过 `output_schema`。`sandbox` 默认 deny，可显式限定 network allowlist、filesystem paths、environment allowlist 和 `max_output_bytes`。

script 必须运行在独立进程/容器中；Python `eval/exec`、Node `vm`、worker thread 或语言内限制不是安全边界。取消必须终止完整进程树。secret 不得进入命令行、默认环境变量、源码或日志。

### 5.3 `set`

`assign` 是 `vars.<name>[.<field>...]` 到 literal/expression 的非空映射。所有右值先在旧 context 中求值并校验，之后原子提交；不可变变量、未知变量或类型不匹配报错。

### 5.4 `if` 和 `switch`

`if` 需要 `condition` 与非空 `then`，可含 `else`。`switch.cases[]` 按顺序包含完整布尔表达式 `when` 和 `steps`；只执行第一个为 true 的 case，否则执行可选 `default`。不采用隐式 equals-only 语义。

### 5.5 `foreach` 和 `while`

`foreach` 必须提供集合表达式 `items`、绑定名 `as`、`max_items` 和 `steps`；可提供 `index_as` 与 `concurrency`。超出 `max_items` 必须失败，不能静默截断。默认 concurrency 为 1，desktop writer action 始终串行。

`while` 必须同时提供布尔 `condition`、`max_iterations`、`timeout` 和 `steps`；条件在每次迭代前求值。达到限制后条件仍为 true 时返回 `LOOP.LIMIT_EXCEEDED`。

### 5.6 `block`、`fail` 和 `return`

- `block.steps` 建立独立 handler/finally/timeout 作用域。
- `fail.error` 至少含稳定 `code` 与人类 `message`，可含 `category`、`retryable`、`effect` 和脱敏 `details`。
- `return.value` 可省略；它立即终止当前 workflow 并进入各层 `finally`。在 error handler 中应通过 `outcome.mode: return` 返回，而不是依赖嵌套 return 的歧义行为。

## 6. 重试、错误处理与 finally

`retry.max_attempts` 包含第一次执行。大于 1 时必须给出 `on.codes` 和/或 `on.categories`；backoff 可为 fixed 或 exponential，并可设置 jitter。自动重试必须同时满足：

1. code/category 命中策略；
2. error 的 `retryable` 为 true；
3. attempt 和父级 deadline 尚未耗尽；
4. action 为 read-only/idempotent，或已确认 `effect: not_applied`。

非幂等 action 超时或连接断开后，若无法证明是否生效，必须产生 `ACTION.UNKNOWN_EFFECT`，状态为 `UNKNOWN_EFFECT`，`retryable: false`、`effect: unknown`，且不得自动重放。恢复运行时也不得把残留 `RUNNING` step 当作未开始，必须先 reconciliation。

`on_error.match` 可匹配 code（支持尾部 `*`）、category 和 effect，并通过 `as` 绑定错误。`outcome.mode`：

- `rethrow`：处理步骤后传播原错误；
- `continue`：必须提供该失败 step 的替代 `output`；
- `return`：以可选 `output` 结束 workflow。

顺序为：执行与 postcondition → 安全 retry → 最近作用域 `on_error` → 向外传播 → 各层 `finally`。原错误存在时 finally 错误进入 `suppressed`，不得覆盖原错误；原流程成功而 finally 失败时返回 `WORKFLOW.FINALLY_FAILED`。总 timeout/取消后，finally 仅拥有 `budgets.cleanup_timeout`。

## 7. 状态与结构化错误

对外 step/workflow 终态固定为：

- `SUCCEEDED`
- `FAILED`
- `TIMED_OUT`
- `CANCELLED`
- `UNKNOWN_EFFECT`
- `SKIPPED`（仅适用于 step）

实现内部可以记录 `PENDING`、`RUNNING` 等非终态，但不得把错误类别伪装成新终态。错误对象至少应有：

```json
{
  "schema_version": "1",
  "code": "ACTION.UNKNOWN_EFFECT",
  "category": "action",
  "message": "Invoke timed out before its effect was verified",
  "phase": "verify",
  "retryable": false,
  "effect": "unknown",
  "location": {"workflow": "example", "step_path": "submit", "attempt": 1},
  "details": {},
  "cause": null,
  "suppressed": [],
  "trace_id": "01K..."
}
```

流程只能匹配稳定 code/category/effect，不能匹配 message。v1alpha1 保留：

`DESCRIPTOR.INVALID`, `DESCRIPTOR.VERSION_UNSUPPORTED`, `CAPABILITY.MISSING`, `CAPABILITY.VERSION_INCOMPATIBLE`, `POLICY.DENIED`, `POLICY.CONFIRMATION_REQUIRED`, `EXPR.EVALUATION_FAILED`, `EXPR.TYPE_MISMATCH`, `LOCATOR.NOT_FOUND`, `LOCATOR.AMBIGUOUS`, `LOCATOR.STALE`, `LOCATOR.APP_IDENTITY_MISMATCH`, `ACTION.PRECONDITION_FAILED`, `ACTION.EXECUTION_FAILED`, `ACTION.POSTCONDITION_FAILED`, `ACTION.TIMEOUT`, `ACTION.UNKNOWN_EFFECT`, `OCR.NO_TEXT`, `OCR.LOW_CONFIDENCE`, `OCR.ENGINE_UNAVAILABLE`, `LOOP.LIMIT_EXCEEDED`, `SCRIPT.SANDBOX_DENIED`, `SCRIPT.EXIT_NONZERO`, `SCRIPT.OUTPUT_INVALID`, `WORKFLOW.TIMEOUT`, `WORKFLOW.CANCELLED`, `WORKFLOW.FINALLY_FAILED`。

provider 可以增加自有大写命名空间。`details`、OCR 文本和截图必须经过 schema、大小限制和脱敏；不得包含 secret，默认不得记录完整截图或完整 OCR 文本。

## 8. 能力清单

Manifest 使用相同 `apiVersion`，`kind: CapabilityManifest`，并包含：

- `metadata.name` 和 `metadata.version`；
- 可选 `runtime`：`kind`、`protocol`、进程/WASM `entrypoint`、args、platforms、host version 与 shutdown grace；
- 可选 provider 级 `permissions`；
- `actions` map，每项含 `contract_major`、相互独立的默认 `effect` 与 `risk`、`input_schema`、`output_schema`，并可声明 permissions、timeout 和结构化 `errors`。Workflow action 和 Manifest action 都把 `effect`、`risk` 作为同级字段；Manifest 仅将 effect 的字段命名为 `default_class`，表示 provider 默认值。

每项 error contract 的 `effect` 为 `not_applied|applied|unknown`。Manifest 只声明能力，不授予权限；workflow 声明、manifest、provider 动态判断和 trusted host policy 必须全部允许。第三方 native provider 必须进程外运行，不能 import/dlopen 到 trusted host。

## 9. v0.x Python 运行时支持矩阵

本规范定义目标语义，不表示当前实现已覆盖全部能力。当前 Python-first v0.x 以 mock/fixture 的闭环集成为目标，Rust-ready 指稳定边界使用 JSON、版本化 action contract 和进程协议，不表示已经存在 Rust core。

| 能力 | v0.x | v1alpha1 目标 |
| --- | --- | --- |
| canonical header、metadata、顺序 steps | 支持/优先 | 完整 |
| `action` + NDJSON process fixture | 支持/优先 | 多 provider、版本解析、schema 校验 |
| `set`, `if`, `fail`, `return` | 首版子集 | 完整语义 |
| `switch`, `foreach`, `while`, `block` | 支持串行有界执行；`foreach.concurrency != 1` 明确拒绝 | 完整且有界 |
| `script` | 默认关闭；仅 Linux bubblewrap + prlimit 可用时执行 | 三端独立进程和 deny-by-default sandbox |
| retry/on_error/finally | 支持基础语义、父子 deadline 与 unknown effect | 持久恢复与取消 |
| budgets、risk/permission/confirmation policy | 支持执行预算与 fail-closed 前置检查 | journal/reconciliation、真实确认 token |
| Windows UIA | 首个进程 driver：list/snapshot/find/focus/invoke/set_value；待 Windows 真机资格测试 | 完整 driver |
| macOS AX / Linux AT-SPI | 仅只读 capability probe | 真实平台 driver |
| OCR engine | 显式 Tesseract 图片 provider；不自行截图 | 受控 frame/capture provenance |

运行时遇到合法但未实现的规范字段必须明确返回 `CAPABILITY.MISSING`、`DESCRIPTOR.VERSION_UNSUPPORTED` 或实现定义的结构化 unsupported 错误；不得静默忽略。

## 10. 示例

`examples/workflows/ocr-error-response.json` 是 canonical JSON 数据模型，`ocr-error-response.yaml` 是等价 authoring 形式。它显式调用 fixture OCR，再根据 OCR 输出的置信度分支：低于阈值时由 workflow 产生 `OCR.LOW_CONFIDENCE`，达到阈值时才调用声明好的按钮动作。该示例使用 fixture 以保持确定性；真实 `vision.ocr` provider 位于 `plugins/ocr_tesseract`，同样不包含隐式截图或坐标点击。
