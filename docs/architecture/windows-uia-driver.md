# Windows 用户界面自动化（UIA）进程驱动

> 状态：首个纵向切片，2026-08-24。该能力仅支持 Windows，并通过可选的
> `comtypes` 绑定使用 `UIAutomationClient`。

## 契约与边界

该能力提供方对外声明 `metadata.name: desktop.windows_uia`，并提供以下 v1
动作：`list_windows`、`snapshot`、`find`、`focus`、`invoke` 和
`set_value`。工作流中的 `uses` 值由能力清单标识、动作键和契约主版本号
组成，例如 `desktop.windows_uia.snapshot@1`。能力清单的
`runtime.platforms` 仅声明 `windows`。

工作进程以“一行一个 JSON 对象”的方式读写数据。输出会转义为 ASCII 安全的线上表示，
因此格式错误的 UTF-16 代理项值不会导致进程终止。stdout 只承载协议输出；有界诊断信息
写入 stderr。请求上限为 1 MiB，响应必须低于宿主进程的 8 MiB 帧上限。
`deadline_ms` 是 Unix epoch 毫秒绝对时间。工作进程会将它转换一次为单调时钟
截止时间，并在枚举窗口、遍历树、解析定位器以及每次原生写操作之前立即检查。
Python 无法安全抢占单次 COM 调用，因此宿主进程的进程超时仍是最终的硬停止边界。

后端依赖是可选的。在非 Windows 系统上，或 `comtypes` /
`UIAutomationCore` 无法初始化时，能力清单协商仍可成功；需要实际访问 UIA 后端的
调用会返回结构化的 `DRIVER.UNAVAILABLE`。这样既能让使用模拟后端的契约测试跨平台
运行，又不会对该能力的平台适用范围作出不实声明。

当前切片明确不支持键盘或指针注入、截图、OCR、可点击点回退机制，也不会
隐式改用其他定位器。它只使用原生 `SetFocus`、`InvokePattern.Invoke` 和
`ValuePattern.SetValue`。

## 归一化观察结果

每个 `snapshot` 结果都具有以下稳定的外层结构：

```json
{
  "snapshot_id": "<worker-generation>:<revision>",
  "revision": 1,
  "app": {},
  "window": {},
  "nodes": [],
  "truncated": false
}
```

节点采用带父节点引用的扁平列表，并包含 `node_id`、`parent_id`、`role`、
`name`、`value`、`states`、`bounds`、`actions` 和 `provenance`。UIA
提供边界信息时，`bounds` 使用物理屏幕像素。密码值不会被读取。原生 COM 元素绝不
跨越进程边界。执行写操作前，真实 UIA 后端会使用 `CompareElements` 确认重新解析得到的
COM 元素仍是原来的原生目标；RuntimeId 和语义指纹也参与目标替换检测，但它们不是持久、
可重放的句柄。

`snapshot_id`、`revision` 和节点 ID 仅在工作进程的当前修订版本内有效。再次获取
快照会使先前的节点引用失效；驱动重启则会改变 `snapshot_id` 中的世代标识部分。

## 定位器与写操作语义

定位器（`locator`）是针对 role、name、value、automation ID、class name、framework ID、
states 和所支持动作的结构化谓词。当前 v0 切片只支持区分大小写的精确字符串比较。
匹配数为零时，解析器返回 `DRIVER.NOT_FOUND`；匹配数大于一时，则返回
`DRIVER.AMBIGUOUS` 以及有界的候选摘要，绝不会直接选择第一个候选项。

`find` 返回一个外层包含 `target` 和匹配节点 `node` 的结果，其中 `target` 是
限定在当前快照内的目标：

```json
{
  "target": {
    "snapshot_id": "...",
    "revision": 1,
    "node_id": "n12"
  },
  "node": {}
}
```

每个写入动作都必须同时携带该 `target` 和原始定位器。派发前，工作进程会验证
目标属于当前快照，以原快照的深度和节点数上限重新抓取 UIA 树，再次解析原始
定位器，并比较原生身份与语义指纹。目标消失、变为多义、无法验证或已被替换时，会在
不调用原生写模式的前提下返回 `DRIVER.STALE_SNAPSHOT`。原快照已被截断，或派发前的
截断快照仍解析出唯一目标时，则返回 `DRIVER.SNAPSHOT_TRUNCATED`；派发前解析已经变为
未找到或多义时，会先归一化为 `DRIVER.STALE_SNAPSHOT`。这些情况都不会执行写操作。
原生写操作一旦进入后端派发边界，即使后端报告失败，也会使当前快照失效，因为工作进程
无法证明派发之后 UI 树仍未改变。当前成功响应不会在派发后重新观察 UI，也不验证
action-specific postcondition；如需确认界面结果，调用方必须获取新快照。

效果分类采取保守策略：`invoke` 的 `effect.default_class` 为 `non_idempotent`；
`focus` 和 `set_value` 的 `effect.default_class` 为 `contextual`。错误使用稳定的
`DRIVER.*` 错误码，并携带有界诊断细节；部分定位失败细节会回显原始定位器或候选节点
摘要，因此这里不提供通用的 secret-redaction 保证。原生调用前可以确认未生效的失败保持
`not_applied`；进入后端派发边界后若发生 `DRIVER.ACTION_FAILED`、
`DRIVER.TIMEOUT` 或未预期异常，则转换为 `DRIVER.UNKNOWN_EFFECT`，不得盲目重放。

## 启动与资格验证

在 Windows 上，使用 `pip install .[windows-uia]` 安装可选依赖，然后运行
`plugins\windows_uia\run.cmd`；也可以向 Python 传入包含
`windows_uia_driver.py` 的显式参数列表。`run.sh` 只用于 POSIX 宿主机上的协议测试和
模拟后端契约测试。

Linux/macOS 测试路径会验证能力清单、不可用状态行为、归一化快照、精确/多义/未找到
解析、目标替换后的过期检测、通过模拟后端执行的三种写入动作语义、截止时间、
结构化错误和输入帧上限；模拟后端测试不等同于真实 UIA 调用资格验证。现有 Windows
条件测试另外只执行一项保守的冒烟测试：依赖缺失时必须返回 `DRIVER.UNAVAILABLE`；后端成功
初始化时，仅要求其能够枚举可见的顶层窗口。真实应用，以及提权/UIPI 边界的资格验证，
仍属于后续独立的平台验收工作。
