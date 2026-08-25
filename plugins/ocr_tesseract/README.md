# Tesseract 光学字符识别（OCR）进程插件

这个可选提供程序通过仓库的 NDJSON 进程协议提供 `vision.ocr.recognize@1`。它只读取
调用方提供的绝对图片路径；绝不会自行截屏，也不会静默回退到屏幕捕获。

运行要求为 `PATH` 中存在 `tesseract` CLI，并使用 Python 3.11 或更高版本。所有图片都必须先
通过 Pillow 的格式、结构、帧数和解码大小验证（`python -m pip install ".[ocr]"`）；不是只有
区域裁剪才需要 Pillow。操作者可通过 `TESSERACT_CMD` 指定二进制文件，
或将 `OCR_TESSERACT_COMMAND` 设为可信的 JSON `argv` 数组（适用于包装器和隔离、可复现的测试）。
这项设置属于高权限的操作者配置，而非工作流输入。

提供程序清单要求 `filesystem.read`。工作流必须在 `requires.permissions` 中声明该权限，
同时宿主操作者还必须通过 `--permission filesystem.read` 显式授予权限；仅声明绝不代表
已授权。该权限覆盖调用方提供的绝对路径。提供程序会将文件复制到私有请求目录中，对该
快照计算哈希，并且只把快照（或其裁剪结果）交给 Tesseract。

请显式注册该插件：

~~~sh
python -m ai_auto_desktop run workflow.yaml \
  --permission filesystem.read \
  --plugin vision.ocr=plugins/ocr_tesseract/run.sh
~~~

在 Windows 上，请直接注册 Python 启动命令（这样也无需依赖 POSIX 命令行环境）：

~~~powershell
py -3 -m ai_auto_desktop run workflow.yaml `
  --permission filesystem.read `
  --plugin "vision.ocr=py -3 plugins/ocr_tesseract/ocr_tesseract_plugin.py"
~~~

同时提供了 `run.cmd`，供接受 `.cmd` 插件命令的命令行环境使用。若系统已注册 Python 启动器，
它会使用 `py -3`；否则会回退到 `PATH` 中的 `python`。

该操作必须且只能接受 `image: {path}` 或 `artifact: {path, media_type?}` 之一。路径本身必须
是绝对路径，并指向不超过 64 MiB 的常规图片文件。解码后的图片还必须同时满足：宽、高各不
超过 20,000 像素，总像素数不超过 40,000,000，且只能有一帧。provider 会尽量从
PNG/GIF/JPEG/TIFF/BMP/WebP/PNM 头部预检尺寸；即使头部布局无法安全解析，也会在交给
Tesseract 前由 Pillow 检查尺寸、帧数并完整解码一次。Pillow 的 decompression-bomb 警告与
错误会转换为结构化
`OCR.IMAGE_LIMIT_EXCEEDED`，结构损坏返回 `OCR.IMAGE_UNSUPPORTED`，未安装验证器则 fail-closed
为 `OCR.IMAGE_VALIDATOR_UNAVAILABLE`。可选字段包括以像素为单位的
`region: {x,y,width,height}`、由 Tesseract 语言 ID 组成的 `languages` 数组、取值范围为零到一的
`minimum_confidence`，以及 `patterns: [{id,value}]`。区域会在运行 Tesseract 前裁剪，返回的
边界坐标仍相对于原始图片。模式值是区分大小写的字面子字符串；系统有意不执行正则
表达式语法。

结果包含提供程序与版本的溯源信息、解析后的源路径及其 SHA-256 摘要、请求的源区域、聚合文本与
置信度、文本行边界和模式匹配结果。找不到 Tesseract 时会返回
`OCR.ENGINE_UNAVAILABLE`；输入无效、图片不可读或签名无效、引擎失败、文本为空、置信度
过低、TSV 格式错误或过大、输出含 NUL 或不是 UTF-8，以及超过截止时间，也都会以结构化
错误返回。每次启动引擎都会创建独立的进程组或会话。Linux 上还强制通过 `prlimit` 限制
地址空间、CPU 时间、输出文件大小和打开文件数；缺少 `prlimit` 时返回
`OCR.ENGINE_ISOLATION_UNAVAILABLE`，不会无约束启动。POSIX 上发生超时或输出溢出时会
终止该进程组；Windows 上会尽力使用 `taskkill /T`（并在宽限期后使用 `/F`）。`stdout`、
`stderr`、TSV 行、单词、文本行、文本、匹配项以及最终 NDJSON 响应均设有硬性上限。

插件会把引擎环境精简到最小必需集合，并在默认情况下设置 `OMP_NUM_THREADS=1`、
`OMP_THREAD_LIMIT=1`，将 Tesseract/libgomp 限制为单线程。这样做是因为 Linux 的
`RLIMIT_NPROC` 语义是按同一 UID 的总任务数（包含线程）计数；在共享主机上把它固定为
`512` 会把其他同 UID 进程和 OpenMP 工作线程一并算进去，导致真实 Tesseract 在启动时因
`libgomp: Thread creation failed` fail-closed。为避免把共享主机上的外部噪声变成插件自身的
可用性故障，provider 不再对引擎追加 `--nproc`，但仍保留其余 `prlimit` 边界、总 deadline、
输出上限和进程组清理。

进程分离、精简环境、OpenMP 单线程约束、进程组清理和 Linux `prlimit` 只是纵深防御，不是完整沙箱：它们不隔离
文件系统、网络、系统调用或同一用户下的其他进程。当前 macOS 与 Windows 路径没有内置的
等价资源沙箱，默认 fail-closed 为 `OCR.ENGINE_ISOLATION_UNAVAILABLE`。只有操作者已经用受控
容器/低权限账户，或可信包装器提供 Job Object、sandbox profile 等外部边界时，才可显式设置
`OCR_ALLOW_UNSANDBOXED_ENGINE=1` 启动；这个开关只确认外部隔离责任，不会自行创建沙箱。
即使设置后仍有硬 deadline、流量上限与进程树终止，也不得把“独立进程”宣称为安全沙箱。

## 结果契约

`recognize` 的输出是闭合对象；宿主会依据 manifest 校验所有字段，不接受未声明字段：

- `source` 恰好包含 `kind`、解析后的绝对 `path`、私有快照的 `digest`、根据文件内容检测的
  `media_type` 和 `size_bytes`。摘要格式固定为 `sha256:<64 个小写十六进制字符>`。
- `lines` 至少包含一项。每项恰好包含非空 `text`、零到一的字符数加权 `confidence`，以及
  原图坐标系中的 `bounds: {x,y,width,height}`。
- `matches` 只包含调用方所声明的区分大小写字面匹配。每项恰好包含 `pattern_id`、`text`、
  聚合 `text` 中从零开始且结尾不包含的 `span: {start,end}`、匹配单词框并集 `bounds` 和
  匹配所覆盖单词的最低 `confidence`。只匹配到分隔符时，`bounds` 为 `null` 且置信度为零。

`minimum_confidence` 是 provider 级硬门槛：低于它时操作返回 `OCR.LOW_CONFIDENCE`，不会产生
正常输出。工作流也可以不设置这个硬门槛，而在显式控制流中同时检查整体置信度、匹配数组和
匹配置信度。

Tesseract 没有返回任何单词时，provider 明确失败为不可重试的 `OCR.NO_TEXT`，而不是伪造空的
成功输出。示例通过只匹配该稳定错误码的工作流级 `on_error`，显式转成
`{decision: no_response, reason: no_text_recognized}`；其他 OCR 或引擎错误仍向调用方传播。

## 显式工作流示例

[`ocr-explicit-image-response.yaml`](../../examples/workflows/ocr-explicit-image-response.yaml)
（另有等价 JSON）调用真实 `vision.ocr.recognize@1` provider。图片路径、目标字面文本、语言列表
和响应阈值全部由调用方显式提供。只有目标命中且整体与匹配置信度均达到阈值时，工作流才通过
`return` 产生 `decision: respond`；低置信度或无命中都返回 `decision: no_response`。示例没有
桌面动作，也不会根据 OCR 边界自动点击坐标。

该示例把 `image_path` 和 `target_text` 声明为 `sensitive: true`，并带有可读的
`ai-auto-desktop.dev/durable-eligibility: denied-sensitive-ocr` annotation。真正的执行保护来自前者：
当前 durable journal 会在创建 run 前 fail-closed，避免把图片路径、目标内容以及后续原始 OCR
结果写入 SQLite；annotation 只供人和外部工具阅读，本身不构成运行时安全边界。这个示例应使用
普通非持久执行入口，直到 durable 层具备字段级脱敏/秘密引用和 OCR 输出污点持久化策略。

~~~sh
python -m ai_auto_desktop run examples/workflows/ocr-explicit-image-response.yaml \
  --input image_path='"/absolute/path/to/status.png"' \
  --input target_text='"A-42"' \
  --input languages='["chi_sim","eng"]' \
  --input minimum_confidence=0.85 \
  --permission filesystem.read \
  --plugin vision.ocr=plugins/ocr_tesseract/run.sh
~~~

`languages` 保持可配置：英文通常使用 `eng`，简体中文与英文可使用 `chi_sim`、`eng`。这些 ID
必须对应操作者机器上已经安装的 Tesseract language data；provider 不会下载或安装语言包。仓库
集成测试使用 fake Tesseract 进程验证语言参数和真实 provider/workflow 协议，因此不依赖测试机
安装任何中文或英文语言包。

桌面 capture/frame provenance 与 pointer/语义点击仍是后续独立能力。未来接入时也必须由 workflow
显式声明 capture 和响应动作、权限、风险与验证条件；本插件不会把文件 OCR 扩展为隐式截屏，
也不会把匹配边界直接转换为点击。
