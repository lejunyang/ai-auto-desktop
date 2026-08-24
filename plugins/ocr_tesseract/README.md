# Tesseract 光学字符识别（OCR）进程插件

这个可选提供程序通过仓库的 NDJSON 进程协议提供 `vision.ocr.recognize@1`。它只读取
调用方提供的绝对图片路径；绝不会自行截屏，也不会静默回退到屏幕捕获。

运行要求为 `PATH` 中存在 `tesseract` CLI，并使用 Python 3.11 或更高版本。区域裁剪还需要
Pillow（`python -m pip install Pillow`）。操作者可通过 `TESSERACT_CMD` 指定二进制文件，
或将 `OCR_TESSERACT_COMMAND` 设为可信的 JSON `argv` 数组（适用于包装器和封闭式测试）。
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

该操作必须且只能接受 `image: {path}` 或 `artifact: {path, media_type?}` 之一。路径必须
指向绝对路径下、不超过 64 MiB 的常规图片文件。可选字段包括以像素为单位的
`region: {x,y,width,height}`、由 Tesseract 语言 ID 组成的 `languages` 数组、取值范围为零到一的
`minimum_confidence`，以及 `patterns: [{id,value}]`。区域会在运行 Tesseract 前裁剪，返回的
边界坐标仍相对于原始图片。模式值是区分大小写的字面子字符串；系统有意不执行正则
表达式语法。

结果包含提供程序与版本的溯源信息、解析后的源路径及其 SHA-256 摘要、请求的源区域、聚合文本与
置信度、文本行边界和模式匹配结果。找不到 Tesseract 时会返回
`OCR.ENGINE_UNAVAILABLE`；输入无效、图片不可读或签名无效、引擎失败、文本为空、置信度
过低、TSV 格式错误或过大、输出含 NUL 或不是 UTF-8，以及超过截止时间，也都会以结构化
错误返回。每次启动引擎都会创建独立的进程组或会话。POSIX 上发生超时或输出溢出时会
终止该进程组；Windows 上会尽力使用 `taskkill /T`（并在宽限期后使用 `/F`）。`stdout`、
`stderr`、TSV 行、单词、文本行、文本、匹配项以及最终 NDJSON 响应均设有硬性上限。
