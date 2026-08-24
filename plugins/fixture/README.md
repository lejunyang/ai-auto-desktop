# 确定性测试能力插件（fixture）

`fixture` 是一个具备确定性、仅依赖标准库的进程插件，用于运行时集成测试。它从
`stdin` 读取 NDJSON，为每个非空请求行向 `stdout` 写入且仅写入一条响应，并将所有
诊断信息发送到 `stderr`。

## 运行

启动器会解析自身所在目录，因此可以从任意工作目录调用：

~~~sh
printf '%s\n' \
  '{"type":"manifest","id":"m1"}' \
  '{"type":"invoke","id":"o1","action":"fixture.ocr@1","args":{"text":"Hello"}}' \
  | plugins/fixture/run.sh
~~~

仅在测试主动协商时传入 `--manifest`。该选项会在读取 `stdin` 前，发送一个包含清单的
`type=manifest` 消息封装。若不使用该选项，宿主也可以通过 `type=manifest` 请求获取同一份清单。

## NDJSON 消息格式

规范的调用请求如下：

~~~json
{"id":"request-1","action":"fixture.invoke@1","args":{"target":"save"}}
~~~

`fixture` 还会忽略 `type=invoke` 和 `deadline_ms` 等无害的消息封装扩展，并接受
JSON-RPC 风格的 `method`/`params`。调用成功时返回包含 `id` 和 `result` 的对象；调用失败时
返回以下结构：

~~~json
{
  "id": "request-1",
  "error": {
    "code": "FIXTURE.REQUESTED",
    "message": "fixture requested an error",
    "retryable": false,
    "data": {}
  }
}
~~~

未提供错误数据时，会省略 `data` 成员。无效 JSON 和无效信封会收到 `id` 为 `null` 的
协议错误；随后仍会继续处理后续输入行。

## 能力清单与操作

握手结果是规范的 `ai-auto-desktop.dev/v1alpha1` `CapabilityManifest`。其元数据名称为
`fixture`、版本为 `1.0.0`，下表操作映射键的 `contract_major` 均为 `1`。因此，宿主会
将它们解析为以下完整 `uses` 标识符：

| 清单键 | 完整操作 ID | 输入 | 结果 |
| --- | --- | --- | --- |
| `ocr` | `fixture.ocr@1` | `text?`、`language?`、`confidence?`、`blocks?`、`result?` | 模拟的文本、语言、置信度与区块，或原样返回 `result` 覆盖值 |
| `invoke` | `fixture.invoke@1` | `target?`、`operation?`、`result?`，以及要回显的值 | 包含 `ok`、`invoked`、`operation`、`target` 和 `args` 的确认信息，或 `result` |
| `transient` | `fixture.transient@1` | `key?`、`failures?`、`code?`、`message?`、`result?` | 每个 `key` 的前 N 次尝试失败，之后成功 |
| `error` | `fixture.error@1` | `code?`、`message?`、`retryable?`、`data?` | 始终返回请求中指定的结构化错误 |
| `sleep` | `fixture.sleep@1` | `seconds?`、`milliseconds?`、`ms?`、`result?` | 休眠后返回 `ok` 和 `sleptSeconds`，或返回 `result` |

`transient` 默认会在 `default` 键下失败一次。其失败使用 `FIXTURE.TRANSIENT`，将
`retryable` 设为 `true`，并在错误数据中包含 `key`、`attempt` 和 `failures`。在同一进程的
生命周期内，不同键使用相互独立的计数器。

`sleep` 接受非负数值的秒数或毫秒数；同时出现多个时长字段时，`seconds` 优先。为了便于
宿主协议实验，插件仍接受操作短名称和若干旧别名，但只有上表中的完整操作 ID 属于
已声明的契约。
