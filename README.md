# ai-auto-desktop 桌面自动化运行时

ai-auto-desktop 是一个面向进程隔离桌面自动化插件的 Python 3.11 工作流运行时。0.1 版刻意保持较小的可信核心：严格的描述文件、禁止函数调用的表达式求值器、有界控制流、结构化失败以及 NDJSON 进程插件。

## 快速开始

```bash
python -m ai_auto_desktop validate examples/workflows/ocr-error-response.yaml
python -m ai_auto_desktop run examples/workflows/ocr-error-response.yaml --plugin fixture=plugins/fixture/run.sh
python -m ai_auto_desktop probe
```

这些命令都只向标准输出写入一个 JSON 对象。描述文件无效或运行失败时，进程以非零状态退出。安装 Python 包后也可直接使用 `ai-auto-desktop` 命令。

输入和插件都通过可重复的赋值参数传入：

```bash
python -m ai_auto_desktop run workflow.yaml \
  --input account_id='"123456"' \
  --input retries=2 \
  --plugin fixture='python plugins/fixture/fixture_plugin.py'
```

每个输入值都必须是 JSON。插件命令会被解析为参数数组，不会交给 shell 执行。仓库内示例会故意返回 `OCR.LOW_CONFIDENCE`，其非零退出码用于演示结构化错误传播。若工作流声明了 `requires.permissions`，必须为每项权限显式传入 `--permission NAME`；宿主绝不会把描述文件中的权限申请视为已经授权。

`probe` 会保守、只读地检查桌面自动化前置条件：Windows 上检查 UIA，macOS 上检查辅助功能与屏幕录制授权，Linux 上分别检查 AT-SPI、X11、Wayland、RemoteDesktop portal、libei 和 uinput。某项能力不可用只会体现在 JSON 报告中，不会让探针命令失败。探针结果是诊断证据，不代表 UI 自动化已经成功。

## 可选 Tesseract OCR

`plugins/ocr_tesseract` 中的进程插件实现了 `vision.ocr.recognize@1`。它只接受显式传入的绝对图片或 artifact 路径，绝不会自行截图。插件支持裁剪声明的像素区域、指定 Tesseract 语言、设置最低置信度，并返回文本行边界和命名的字面文本匹配。工作流还必须在 `requires.permissions` 中声明 `filesystem.read`，注册示例：

```bash
python -m ai_auto_desktop run workflow.yaml \
  --permission filesystem.read \
  --plugin vision.ocr=plugins/ocr_tesseract/run.sh
```

Tesseract 是可选的系统依赖；所有 OCR 请求都需要 Pillow 完成图片格式、尺寸、帧数与完整
解码校验，区域裁剪也由 Pillow 执行。依赖缺失、低置信度等情况都会返回结构化 `OCR.*`
错误。OCR 输出始终是不可信数据，只有后续显式的 `if` 或 `switch` 才能据此选择响应动作。
推荐的新路径是 `vision.ocr.recognize_artifact@1`：它只接受同一 run 中 Host 管理的
`ArtifactRef`，图片字节通过私有 side channel 传入，结果不会暴露 Host 路径。原
`recognize@1` 继续兼容显式绝对路径，并单独要求 `filesystem.read`。仓库中的
`linux-capture-ocr-decision.yaml` 演示了受控目标截图后显式 OCR、再用 `if` 判断的流程；
示例不包含点击动作，也不会把 OCR bounds 自动转换为坐标操作。

## Windows 用户界面自动化驱动（UIA）

`plugins/windows_uia` 是首个原生 UIA 驱动实现；Windows 原生 CI 只在手动布尔输入或
可信 push 的 `[windows-native]` 提交标记下显式运行。
在 Windows 上，它使用可选的 `comtypes` 绑定枚举窗口并归一化有界的 UIA Control View。
驱动支持精确定位，以及原生 `SetFocus`、`InvokePattern.Invoke`、`ValuePattern.SetValue`、
显式 `type_text` Unicode 键盘后备和显式 `pointer_click` 左键后备；每次写操作都会在派发前
重新抓取快照、解析目标并核对原生元素身份。安装并注册：

```powershell
pip install .[windows-uia]
python -m ai_auto_desktop run workflow.yaml `
  --permission desktop.observe `
  --permission desktop.input `
  --plugin "desktop.windows_uia=plugins\windows_uia\run.cmd"
```

读取类工作流声明 `desktop.observe`，写操作还要声明 `desktop.input`，两者都需要宿主显式授权。`type_text` 不会从 `set_value` 自动启用，只接受有界普通文本并禁止密码/protected 元素；它受 UIPI、完整性级别与前台焦点限制。`pointer_click` 也不会由其他动作失败隐式启用，只能点击 fresh UIA 节点 bounds 的中心，并要求原生 hit-test 仍命中该元素。该驱动不截图、不执行 OCR。
真实 Windows fixture 与 CI 已包含 `set_value → postcondition.observe(snapshot) → condition`
闭环；在非 Windows 主机上只做契约测试，不能替代真实 UIA runner。

## Linux KDE/X11 AT-SPI 驱动

`plugins/linux_atspi` 提供 `list_applications`、`snapshot`、`find`、`focus`、
`invoke`、`set_text`、显式 `type_text`、显式 `pointer_click`、`toggle`、`expand` 和 `collapse`。当前 v0 仅在进程环境明确为 KDE + X11 时启用；优先使用
`Atspi 2.0` typelib，缺失时用 Gio/D-Bus fallback 提供只读枚举和快照。Gio fallback
不会伪装写能力，全部写动作都会返回 `DRIVER.ACTION_UNSUPPORTED`。注册方式：

```bash
python -m ai_auto_desktop run workflow.yaml \
  --permission desktop.observe \
  --plugin desktop.linux_atspi=plugins/linux_atspi/run.sh
```

本机 KDE Plasma 5.27/X11 已通过真实 AT-SPI registry、进程协议和有界 snapshot smoke；
自有 GTK3 fixture 验证 focus、set_text、invoke、toggle、expand/collapse，自有 Qt 5
Widgets fixture 也验证 snapshot/find/focus/set_text/invoke；隔离 X11/AT-SPI fixture 还
验证两套 toolkit 的 `type_text` 经 XTEST 输入 UTF-8 后由 fresh snapshot 观察。
发行版 KCalc 22.12.3 还在禁用 TCP、使用一次性 Xauthority 的私有 Xvfb/KWin，以及
私有 D-Bus 与临时 HOME/XDG 中，通过精确按钮定位分别以原生 AT-SPI `Press` 和显式
`pointer_click` 完成 `1+2=3`，并从 fresh snapshot 读取同一显示控件的结果 `3`。pointer
路径只使用 fresh 语义 bounds 的中心点，并经过 AT-SPI hit-test 与 X11 PID/focus 复核；
两条路径都不使用 OCR 或截图。
真实应用 Dolphin 22.12.3、Konsole 22.12.3、System Settings 5.27.5 以及自有 Qt
Quick/QML fixture 的初始窗口已完成只读资格验证，分别取得 358、352、256 与 5 个未截断
节点，且写动作派发数为零；Dolphin 只打开临时空目录。这仍不等于“任意 KDE/QML 应用已经
支持”。自有 QML fixture 还通过 exact AT-SPI `Press` 和 fresh snapshot 状态变化验证，未使用
键盘注入、OCR 或坐标；第三方 QML 页面、多窗口和动态页面仍需单独验证。显式
`pointer_click` 已在私有 Xvfb 的自有 GTK3/Qt5 fixture 和真实 KCalc 上通过 XTEST 点击及
fresh snapshot 后置条件；它只接受语义节点中心点，并要求 AT-SPI point hit 落在目标子树
内。除显式
`type_text`/`pointer_click` 外不会注入键鼠；驱动不截图、不执行 OCR。

## macOS 真机自测包

`tests/macos/package-source.sh` 可生成能复制到 Intel 或 Apple Silicon Mac 的自包含源码包。
该套件使用
系统 `xcrun`/`swiftc` 构建固定 bundle ID 的 AppKit fixture 与 AX runner，限定在自有
fixture PID 内验证有界 AX 遍历、精确 identifier、focus、set value、press、显式
`type_text` 的 ASCII、中文和非 BMP Unicode 输入，以及由 fresh AX bounds 推导中心点的显式
`pointer_click`；每个动作后都重新观察。套件还会在
派发前检查 Secure Event Input，并验证 secure text 目标被拒绝。默认不弹权限请求；只有
显式参数才请求 Accessibility。Screen Recording 仅做 preflight，整个测试不截图。
运行方式与回传归档格式见
`docs/testing/macos-fixture.md`。

## Host 管理的图片制品

运行时支持在 capability manifest 的 action 上声明 `artifacts.inputs` 和
`artifacts.outputs`。工作流和 NDJSON 控制面只携带闭合的 `ArtifactRef`（ID、SHA-256、
媒体类型和字节数），不会暴露 Host 的文件路径、存储键或文件描述符。POSIX 上，实际图片
字节通过进程启动时建立的私有 Unix socket side channel 传输；每个调用和 slot 都有不可复用
token，并受同一个绝对 deadline、单 slot 大小及单次调用总量约束。Host 对输出做完整摘要、
图片结构和配额校验，并在 output schema 通过后一次性发布全部输出；任何一步失败都会回滚。

Python 调用方可以为一次 run 显式传入 `ArtifactStore`，先用
`runner.import_artifact_bytes(...)` 导入可信图片，再将返回的 ref 放入 action 输入；返回的
`RunResult.resolve_artifact(...)` 可在结果关闭前读取输出。当前存储与 side-channel 后端明确是
POSIX-only；Windows 的公共 ref/manifest 契约已统一，但原生内容通道仍待 named pipe 后端。
fixture 的 `fixture.artifact_copy@1` 只用于验证这条无路径传输链，不能视作截图或 OCR 能力。

## 描述文件与运行时

只接受以下规范标识：

```yaml
apiVersion: ai-auto-desktop.dev/v1alpha1
kind: Workflow
metadata:
  name: hello
budgets:
  max_duration: 30s
  max_executed_steps: 20
steps:
  - id: done
    type: return
    value: hello
```

核心对象拒绝未知字段。步骤 ID 在分支、错误处理器和清理步骤中也必须全局唯一。当前支持 `action`、`set`、`if`、`switch`、`foreach`、`while`、`block`、`script`、`fail` 和 `return`。

完整表达式模板会保留结果类型，嵌入普通文本的表达式则转换成字符串。表达式禁止所有函数和方法调用。只读与幂等动作可以重试明确标记为可重试的结构化错误。非幂等或上下文相关动作在请求已经写出后超时，会返回 `ACTION.UNKNOWN_EFFECT`，且绝不会自动重放。

## Python 编程接口

```python
from ai_auto_desktop import WorkflowRunner, load_descriptor

workflow = load_descriptor("workflow.yaml")
result = WorkflowRunner(
    workflow,
    plugins={"fixture": ["plugins/fixture/run.sh"]},
).run({"name": "Ada"})
print(result.to_dict())
```

`RunResult.status` 可能是 `succeeded`、`failed`、`timed_out`、`cancelled` 或 `unknown_effect`。错误对象包含稳定的 `code`、`category`、`retryable`、`effect`、`details`、`cause`、`suppressed` 和位置信息。

需要跨进程查询、控制和恢复 run 时，可以使用 SQLite journal、`RunService` 与
`DurableExecutor`。命令行提供 `start`、`resume`、`status`、`list`、`events`、`pause` 和
`cancel`，每次只向 stdout 输出一个 JSON 文档：

```bash
python -m ai_auto_desktop start workflow.yaml --journal runs.sqlite3 --run-id demo
python -m ai_auto_desktop status demo --journal runs.sqlite3
python -m ai_auto_desktop pause demo --journal runs.sqlite3
python -m ai_auto_desktop resume demo workflow.yaml --journal runs.sqlite3
python -m ai_auto_desktop events demo --journal runs.sqlite3
```

持久执行默认拒绝所有 action。只有在 `start` 和后续 `resume` 时显式传入
`--durable-actions read-only`，才会启用受限的只读 action 通道；例如还需按普通运行方式
注册对应插件：

```bash
python -m ai_auto_desktop start readonly.yaml --journal runs.sqlite3 \
  --run-id read-demo --durable-actions read-only \
  --plugin records='python path/to/read_only_provider.py'
python -m ai_auto_desktop resume read-demo readonly.yaml --journal runs.sqlite3 \
  --durable-actions read-only \
  --plugin records='python path/to/read_only_provider.py'
```

Python API 示例：

```python
from ai_auto_desktop import JournalStore, RunService

with JournalStore("runs.sqlite3") as journal:
    service = RunService(journal)
    run = service.get("run-id")
    service.request_pause(run.run_id)
```

`request_pause`、`request_resume` 和 `request_cancel` 只原子记录 operator 的期望；
runner 在持有有效 owner lease 的安全点应用状态。暂停会原子释放 lease，恢复后必须重新
claim。状态完成与并发的暂停/取消请求都以 `desiredState` 做 CAS，避免覆盖 operator 的控制
意图。当前受限 v0 只接受 `max_concurrency=1`、顶层未显式声明 `depends_on` 的隐式串行链。
即使启用只读通道，action 也必须是顶层、无条件且只有一次 attempt，不能带 `if`、
`precondition`、`postcondition`、retry、step handler 或 step `finally`；嵌套 action、script、
非 `read_only` action 及敏感输入/输出仍会失败关闭。

可持久 action 要求 provider manifest 中的 action 合约和 descriptor action 都将 input、output、
error sensitivity 显式声明为 `public`。provider 还要声明稳定的
`durability.checkpoint_fields` 白名单，descriptor 必须通过 `checkpoint.output` 选择 `project`
字段或 `omit` 输出。创建 run 前会验证 manifest、有效 effect、错误合约、sensitivity、投影和
静态 policy；错误合约必须非空且全部声明为 `not_applied`。预检不会提前求值可能引用前序
`steps.<id>.output` 的动态 `with`；实际输入在派发前求值并校验。

符合条件的 action 会在 provider dispatch 前持久化 `action_intent` v2；其中只保存 operation、
attempt、deadline 和 provider/contract/projection/input binding 摘要，不保存原始输入。恢复时会
重新验证绑定，并可安全重放只读 action。成功后只把 `project` 选中的字段写入 checkpoint，
`omit` 则不保存 provider 输出；原始响应不会进入持久状态。普通
`between_top_level_steps` 仍是安全恢复点，而没有合法 intent 的 `in_top_level_step` 或
`finalizing` 仍会零派发地终结为 `unknown_effect`。原始绝对 deadline 与已消耗 attempt 数不会
因恢复而重置。

## 进程插件协议

插件通过标准输入和标准输出交换“一行一个 JSON 对象”的 NDJSON。启动时既支持插件主动发送 Manifest，也支持带请求 ID 的 Manifest 请求。调用请求包含 `type`、`id`、`action`、`args` 和绝对时间戳 `deadline_ms`；响应必须带匹配的 ID，并包含 `result` 或结构化 `error`。宿主会限制输出、持续读取标准错误，并在超时或协议错误后终止插件进程组。可运行的确定性测试插件位于 `plugins/fixture`。

## 脚本与安全

只有显式传入 `--allow-scripts` 或 `allow_scripts=True` 才能执行 `script` 步骤。0.1 版仅在 Linux 且 bubblewrap 与 `prlimit` 可用时执行脚本：工作进程通过标准输入接收 JSON，看不到宿主 home 与 `/etc`，使用独立网络/PID 命名空间，并受到墙钟时间、CPU、地址空间、文件大小和输出上限约束。其他平台在实现等价操作系统隔离前，一律返回 `SCRIPT.SANDBOX_UNAVAILABLE`。

当前范围包含 Windows UIA、macOS AX 与 Linux KDE/X11 AT-SPI 纵向切片，但尚未完成
Windows/macOS 真机应用矩阵或任意 KDE 应用的产品级资格验证。运行时已有 SQLite run/event
journal、owner lease fencing，以及受限串行计划在顶层安全点的 checkpoint 恢复；控制仍是
安全点生效的合作式机制，已派发的 OS/插件调用不能被异步强杀。durable 模式默认拒绝 action，
但显式 opt-in 后可执行并恢复经过投影约束的顶层只读 action；写 action、script 和敏感数据仍
拒绝。DAG runtime 会并发独立的非桌面
`read_only` action，其余写动作与控制流保持全局独占。系统 secret store、确认 token、完整
污点传播和跨进程 single-desktop-writer 仍未实现。v0 宿主已经执行声明式风险/权限检查，
校验 Manifest 与 action 输入输出契约，并可通过 `postcondition.observe` 获取动作后真实观察。
