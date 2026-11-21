# 提交中断后，核对回执再续跑

普通 `aad run --journal` 的 NDJSON 可以定位断点，但没有足够信息恢复；**不能拿旧日志跳到某一步**。需要续跑的批次，从第一次运行就用 `--recovery-log` 创建新的本地 SQLite 文件。它和 `aad start/resume` 的 durable store 是两个不同的入口。

## 第一次执行

以下假设流程的提交步骤叫 `submit`，输入 `order` 是业务系统中唯一、不可复用的订单号。桌面或插件所需权限、端口和提供方参数仍要照常提供。

```powershell
$runArgs = @('run', './orders.yaml', '--inputs', '{"order":"ORD-80"}',
             '--permission', 'desktop.observe', '--permission', 'desktop.input')
aad @runArgs --recovery-log ./orders.db --business-key 'submit=/inputs/order'
```

`STEP=POINTER` 是该步骤表达式作用域中的 JSON Pointer，值必须是非空字符串。例如 `foreach ... as: row` 中用 `submit=/row/order_id`，同一步骤在一次运行中不能重复提交相同键。也可以完全省略 `--business-key`，届时用显式回执。

## 中断以后

先确认原执行器已停，保持原业务系统、租户和账户不变，再运行 `aad review ./orders.db`。不要从头新建一个 run。检查 `status`：

- `succeeded`：已完成，不允许再续跑；`reconciliations` 给出核对过的步骤、票号和回执编号，不回显动作输出。
- `between_actions`：没有在途动作；用 `aad @runArgs --recover-from ./orders.db` 继续。
- `needs_receipt`：`pending` 给出步骤、派发票号及可选业务键。**先向业务系统核实**。已匹配的旧动作不会再派发，后续步骤仍需合法输入和权限。

**按键确认已提交。** 从权威业务记录导出 JSON 数组（不是用户原来打算提交的数据）：

```json
[{"step":"submit","key":"ORD-80","receipt":"ERP-9472","decision":"applied","output":{"order_id":"ERP-9472"}}]
```

匹配必须唯一。条目中的 `output` 是下游步骤应该看到的原动作结果，不是整条审计记录；须符合提供方输出契约，没有结果时显式写 `null`（若契约允许）。确认后：

```powershell
aad @runArgs --recover-from ./orders.db --receipt ./erp-receipts.json
aad review ./orders.db
```

**显式确认一次派发。** 若没有可用的业务键，提交一个 JSON 对象，用 review 给出的 `run_id`、`pending.step` 和 `pending.ticket` 绑定这一次派发：

```json
{"run_id":"<run_id>","step":"submit","ticket":"<pending.ticket>","receipt":"ERP-9472","decision":"applied","output":{"order_id":"ERP-9472"}}
```

只有业务系统**明确证明请求未生效且不会稍后生效**时，才可改用 `"decision":"not_applied"`，提供独立凭据编号并省略 `output`。这会授权一次新派发；新票号写入日志后旧凭据即失效。查不到、超时、处理中、多条匹配都不是未提交证明。工具校验绑定关系，不替你验证业务系统或回执签名。

## 保护范围

- 计划摘要、原始输入对象摘要、运行时版本、日志校验链/末尾标记都必须一致。恢复沿原流程重算表达式和循环，逐次核对动作、契约和已解析参数，用已落盘的输出重建作用域，再执行剩余动作；不是按步骤名跳转。恢复进程有新的单次运行时长预算。
- 每次外部调用前持久化意图，得到结果后单独提交完成记录。运行/恢复时独占日志。使用可靠的本地文件系统和原路径，不编辑、替换或回滚数据库，不用备份重复续跑。校验链能发现损坏，不是防恶意篡改的签名。
- 日志存储完整动作输出和业务键。只用于可写入本地私有目录的数据，单条记录和回执文件上限均为 4 MiB。拒绝声明为敏感的数据、artifact action、script、retry、handler、finally 和 action postcondition。不要跨进程复用页面句柄、文件描述符等临时资源；外部环境变化时，应重新观察并核实。
- 不把恢复变为自动重试，也不提供跨日志、跨操作者的全局去重。业务端支持幂等键时仍应使用。

回归：`cargo test --locked -p aad-runtime --test engine reconcile_requires_matching_evidence`；构建 `aad-cli` 后运行 `node --test tests/workflow/recovery.test.mjs`，用业务侧独立记录核对“提交已落地、执行器被杀、回执未返回”的实际 CLI 续跑。
