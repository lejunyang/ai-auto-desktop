# Tesseract OCR 原生能力

`vision.ocr` 已由 [`aad-ocr`](../../crates/aad-ocr) 以 Rust 实现，并由 CLI/MCP 自动注册；
本目录不再包含需要单独启动的 Python plugin。运行时只要求 `PATH` 中可找到 `tesseract`
CLI 及工作流所请求的语言数据。

provider 提供两个显式动作：

- `vision.ocr.recognize_artifact@1`：推荐入口，从当前 run 的 `ArtifactStore` 消费
  `ArtifactRef`，不会暴露 Host 路径；
- `vision.ocr.recognize@1`：兼容既有工作流的绝对路径入口，需要
  `filesystem.read` 权限。

两个动作都不会自行截图，也不会在 locator 失败后隐式执行。输入经过 Rust image decoder
的格式、单帧、尺寸和像素上限校验；Tesseract 子进程受绝对 deadline、stdout/stderr 上限、
进程树回收和平台隔离策略约束。Linux 需要 `prlimit`；其他平台默认失败关闭，只有部署者已
提供外部隔离时才可显式设置 `OCR_ALLOW_UNSANDBOXED_ENGINE=1`。可通过 `TESSERACT_CMD`
或可信的 `OCR_TESSERACT_COMMAND` JSON argv 覆盖引擎命令。

普通运行示例：

```sh
cargo run --locked -p aad-cli -- run \
  examples/workflows/ocr-explicit-image-response.yaml \
  --input image_path='"/absolute/path/to/status.png"' \
  --input target_text='"A-42"' \
  --input languages='["chi_sim","eng"]' \
  --input minimum_confidence=0.85 \
  --permission filesystem.read
```

结果包含 source provenance、聚合文本/置信度、行 bounds 和字面 pattern matches。无文本、
低置信度、损坏/超大图片、引擎失败、超时与输出超限都返回稳定结构化错误。
