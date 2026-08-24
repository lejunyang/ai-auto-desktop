# macOS Accessibility 真机 fixture 测试

仓库提供 `tests/macos/` 自包含测试套件，供用户在真实 Intel 或 Apple Silicon Mac 的
可交互登录会话中执行。详细命令和授权步骤见 `tests/macos/README.md`。

测试通过系统 `xcrun`/`swiftc` 构建固定 bundle ID、ad-hoc 签名的 AppKit fixture 和
AX runner。授权可用时，runner 只从 fixture PID 创建 `AXUIElement`，进行有界遍历与唯一
定位，然后验证 `AXFocused`、`AXValue`、`AXPress`；每个动作的成功都必须由新一次 AX
读取确认，不能只依赖写 API 的返回码。套件也按 `type_text` 的显式 CGEvent 路径分段覆盖
ASCII、中文和非 BMP emoji；每段输入前重新解析 AX 目标并确认 focus/前台，输入后以 fresh
snapshot 验证累计值。每段还在首事件前记录 `IsSecureEventInputEnabled()`，启用时 fail closed。
`NSSecureTextField` 负向 case 在任何事件发布前拒绝 secure text；报告只声明事件已提交，不把
void `postToPid` 当成接收确认。
每个被访问的 AX element 还会设置消息超时，避免
目标进程无响应时无限等待。
runner 由 LaunchServices 以固定 `.app` 身份启动，并将报告原子写入结果目录；外层脚本
根据报告中的状态归一退出码，而不把 `open` 的状态误当成 AX 测试结果。

安全和权限边界如下：

- 默认只调用 `AXIsProcessTrusted()`，不会静默弹窗；只有用户显式传入
  `--prompt-accessibility` 才调用带 prompt 的 trust check。
- 调用 `CGPreflightScreenCaptureAccess()` 只读取状态；不请求屏幕录制授权，也不截图。
- AX 树限定于测试套件自行启动的 fixture 进程，默认最大深度 8、最多 128 个节点。
- 可回传归档只包含结构化 JSON、签名/架构证明、SHA-256 清单和隐私说明，不含屏幕内容或其他应用数据；签名证明不保存绝对路径。
- 结果归档会把 owner/group 固定为 root/0、mtime 固定为 2000-01-01 UTC，并把普通文件
  mode 固定为 `0644`；同时关闭 ACL、file flags、xattr、AppleDouble 和 gzip header 时间/文件名。
  它只接受已知的 macOS bsdtar/libarchive 或 GNU tar，归一化能力不可用时会 fail closed。
- `identity.txt` 强制包含 Swift 版本、identity stability，以及 runner/fixture 各自非空的
  designated requirement、Identifier、CDHash、architectures 和 SHA-256；采集命令失败不会被
  `sed`/`awk` 等管道末端掩盖，也不会发布半份证明。
- 缺少 macOS、Command Line Tools 或授权时输出结构化 `unsupported`，不会伪造通过。

固定 bundle ID 与稳定构建路径可以减少 TCC 身份漂移，但 ad-hoc 签名在重编译后不保证
保留授权，其身份稳定性只能标为 `ephemeral`；长期固定 runner 应通过
`MACOS_TEST_CODESIGN_IDENTITY` 使用同一 Developer ID 签名。带 prompt 的 AX API 是异步的，
本次运行仍以普通 `AXIsProcessTrusted()` 的即时结果为准。

这是一条真机 fixture 验证链路，不等同于对任意第三方应用、锁屏、安全输入、跨用户会话
或完整 macOS 平台兼容性的资格声明。Linux 机器只能对 shell 和源码结构进行静态检查。
真机 kit 已实现 `desktop.macos_ax.type_text@1` 对应的 Unicode 和 secure text case；在真实 Mac
生成 `passed` 报告前，仍不能把它写成已经通过资格验证。该验证只需 Accessibility，不请求
Screen Recording，不加入 pointer 或 `set_value` 自动 fallback。fixture、runner 与说明均已纳入
`tests/macos/SOURCE_PACKAGE_FILES.txt` 白名单。

需要把真机套件交给仓库外的 Mac 时，在 `tests/macos/` 执行：

```sh
./package-source.sh /absolute/path/macos-ax-testkit-source.tar.gz
```

该命令按 `SOURCE_PACKAGE_FILES.txt` 白名单生成平铺的规范化源码包，不包含构建结果。接收方
解压到空目录后即可运行 `./run.sh`。最终对外交付归档应由发布负责人从待交付 revision 生成。
