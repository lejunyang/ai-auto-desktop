# macOS Accessibility 自包含测试套件

本目录提供一个只依赖 macOS 系统 `xcrun`、`swiftc` 和系统 framework 的真机测试套件。
它会构建两个使用固定 bundle ID 的 ad-hoc 签名 `.app`：

- `dev.ai-auto-desktop.testkit.fixture`：只包含原生 AppKit 文本框、按钮和状态文本；
- `dev.ai-auto-desktop.testkit.ax-runner`：只遍历上述 fixture 进程的 AX 树。

在 Intel 和 Apple Silicon Mac 上执行：

```sh
./run.sh
```

默认运行不会弹出辅助功能授权请求。若报告为 `unsupported`，可明确请求一次系统授权，
完成“系统设置 → 隐私与安全性 → 辅助功能”操作后重新运行：

```sh
./run.sh --prompt-accessibility
./run.sh
```

`run.sh` 会在 stdout 输出单个 JSON 文档，并在 stderr 打印结果归档路径。退出码为：
`0` 通过、`1` 测试或构建失败、`3` 当前环境不支持/未授权、`64` 参数错误。
结果目录默认位于 `tests/macos/results/`，其中的 `macos-ax-test-result.tar.gz` 可直接回传。
归档只含 `report.json`、`identity.txt`、`SHA256SUMS` 和隐私说明，不含截图、屏幕像素、构建日志、用户名、主机名，
也不读取 fixture 之外的应用。
`identity.txt` 仅保存 bundle 的 designated requirement、Identifier、TeamIdentifier、
CDHash、Mach-O 架构和可执行文件 SHA-256，不保存绝对路径。
其中 Swift 版本、identity stability，以及 runner/fixture 各自的 designated requirement、
Identifier、CDHash、架构、SHA-256 都是必填证明；任何工具失败、字段缺失或空值都会让构建
失败，不会产生半份 `identity.txt`。

结果归档使用固定的成员顺序、UTC 时间（2000-01-01 00:00:00）、文件权限和 root/0
owner/group，并移除 ACL、file flags、扩展属性与 macOS AppleDouble 元数据；gzip header 也不
记录原文件名或时间。脚本分别适配 macOS 系统 bsdtar/libarchive 和 GNU tar；无法识别 tar
实现或归一化参数不被支持时会拒绝生成归档。

runner 始终调用 `CGPreflightScreenCaptureAccess()` 记录当前状态，但绝不调用授权请求 API，
也不执行截图。Accessibility 可用时，它会启动自有 fixture，在最大深度 8、最大节点数
128 的边界内唯一定位控件，依次验证 focus、set value、显式 Unicode keyboard input 和
press，并在每个动作后重新遍历读取。键盘输入分段覆盖 ASCII、中文和非 BMP emoji；每段都
重新解析目标、先确认 AX focus 与 fixture 仍在前台，使用 `CGEventKeyboardSetUnicodeString`
定向提交到 fixture PID，再从 fresh AX snapshot 验证累计值；报告使用 `event_submitted`，不把
void `postToPid` 描述为已接受。每段首事件前还记录 `IsSecureEventInputEnabled()` 结果并在启用
时拒绝提交。套件还通过 `NSSecureTextField`
确认 secure text 在任何键盘事件前被拒绝。
runner 通过 LaunchServices (`open -n -W`) 启动，使 TCC 对应实际 `.app` 身份；结果由 runner
原子写入指定文件，外层脚本不把 `open` 的退出码当作测试结论。

这些 case 对齐进程驱动新增的显式 `desktop.macos_ax.type_text@1`，但在真实 Mac 回传
`passed` 报告之前仍只代表测试实现已就绪。它只需要 Accessibility，不需要 Screen Recording，
不注入 pointer，也不是 `set_value` 的自动 fallback。fixture、runner 和本 README 均在
`SOURCE_PACKAGE_FILES.txt` 白名单内；若未来增加文件，必须同步更新该白名单。

如缺少工具，先运行：

```sh
xcode-select --install
```

为减少 TCC 中的应用身份和路径变化，默认构建目录是稳定的 `tests/macos/.build/`；
源码未变化时不会重复编译。不要在授权与复测之间删除该目录。固定 bundle ID 加 ad-hoc
签名的 `identity_stability` 是 `ephemeral`，重编译后可能需要重新授权；长期固定测试节点应
设置 `MACOS_TEST_CODESIGN_IDENTITY`，始终使用同一 Developer ID 签名。

## 制作可搬运源码包

从仓库中的 `tests/macos/` 运行：

```sh
./package-source.sh /absolute/path/macos-ax-testkit-source.tar.gz
```

脚本只打包 `SOURCE_PACKAGE_FILES.txt` 白名单中的源码与脚本，不包含 `.build/`、`results/`
或仓库其他文件。归档是平铺结构，可复制到仓库外直接执行：

```sh
mkdir macos-ax-testkit
tar -xzf macos-ax-testkit-source.tar.gz -C macos-ax-testkit
cd macos-ax-testkit
./run.sh
```

