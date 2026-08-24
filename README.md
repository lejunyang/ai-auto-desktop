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

Tesseract 是可选的系统依赖；只有区域裁剪需要 Pillow。依赖缺失、低置信度等情况都会返回结构化 `OCR.*` 错误。OCR 输出始终是不可信数据，只有后续显式的 `if` 或 `switch` 才能据此选择响应动作。

## Windows 用户界面自动化驱动（UIA）

`plugins/windows_uia` 是首个真实原生桌面驱动。在 Windows 上，它使用可选的 `comtypes` 绑定枚举窗口并归一化有界的 UIA Control View。驱动支持精确定位，以及原生 `SetFocus`、`InvokePattern.Invoke` 和 `ValuePattern.SetValue`；每次写操作都会在派发前重新抓取快照、解析目标并核对原生元素身份。安装并注册：

```powershell
pip install .[windows-uia]
python -m ai_auto_desktop run workflow.yaml `
  --permission desktop.observe `
  --permission desktop.input `
  --plugin "desktop.windows_uia=plugins\windows_uia\run.cmd"
```

读取类工作流声明 `desktop.observe`，写操作还要声明 `desktop.input`，两者都需要宿主显式授权。该驱动不截图、不执行 OCR，也不注入键盘或鼠标输入。
真实 Windows fixture 与 CI 已包含 `set_value → postcondition.observe(snapshot) → condition`
闭环；在非 Windows 主机上只做契约测试，不能替代真实 UIA runner。

## Linux KDE/X11 AT-SPI 驱动

`plugins/linux_atspi` 提供 `list_applications`、`snapshot`、`find`、`focus`、
`invoke` 和 `set_text`。当前 v0 仅在进程环境明确为 KDE + X11 时启用；优先使用
`Atspi 2.0` typelib，缺失时用 Gio/D-Bus fallback 提供只读枚举和快照。Gio fallback
不会伪装写能力，三个写动作都会返回 `DRIVER.ACTION_UNSUPPORTED`。注册方式：

```bash
python -m ai_auto_desktop run workflow.yaml \
  --permission desktop.observe \
  --plugin desktop.linux_atspi=plugins/linux_atspi/run.sh
```

本机 KDE Plasma 5.27/X11 已通过真实 AT-SPI registry、进程协议和有界 snapshot smoke。
当前 Qt System Settings 没有注册到 registry，因此 Qt bridge 和真实写动作仍未通过资格验证；
这与“Linux 驱动不存在”不同，也不等于“任意 KDE 应用已经支持”。驱动不注入键鼠、不截图、
不执行 OCR。

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

## 进程插件协议

插件通过标准输入和标准输出交换“一行一个 JSON 对象”的 NDJSON。启动时既支持插件主动发送 Manifest，也支持带请求 ID 的 Manifest 请求。调用请求包含 `type`、`id`、`action`、`args` 和绝对时间戳 `deadline_ms`；响应必须带匹配的 ID，并包含 `result` 或结构化 `error`。宿主会限制输出、持续读取标准错误，并在超时或协议错误后终止插件进程组。可运行的确定性测试插件位于 `plugins/fixture`。

## 脚本与安全

只有显式传入 `--allow-scripts` 或 `allow_scripts=True` 才能执行 `script` 步骤。0.1 版仅在 Linux 且 bubblewrap 与 `prlimit` 可用时执行脚本：工作进程通过标准输入接收 JSON，看不到宿主 home 与 `/etc`，使用独立网络/PID 命名空间，并受到墙钟时间、CPU、地址空间、文件大小和输出上限约束。其他平台在实现等价操作系统隔离前，一律返回 `SCRIPT.SANDBOX_UNAVAILABLE`。

当前范围包含 Windows UIA 与 Linux KDE/X11 AT-SPI 纵向切片，但尚未完成 Windows 真机
应用矩阵、Linux Qt 写动作或 macOS AX 的产品级资格验证；也没有持久化与恢复、secret 存储、
桌面并发写入、确认 token 或完整污点执行。v0 宿主已经执行声明式风险/权限检查，校验进程
Manifest 与 action 输入输出契约，并可通过 `postcondition.observe` 在动作后重新获取真实观察。
