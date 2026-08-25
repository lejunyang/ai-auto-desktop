# macOS Accessibility 自包含测试套件

本目录提供一个只依赖 macOS 系统 `xcrun`、`swiftc` 和系统 framework 的真机测试套件。
它会构建两个使用固定 bundle ID 的 ad-hoc 签名 `.app`：

- `dev.ai-auto-desktop.testkit.fixture`：只包含原生 AppKit 文本框、按钮和状态文本；
- `dev.ai-auto-desktop.testkit.ax-runner`：只遍历上述 fixture 进程的 AX 树。

在 Intel 和 Apple Silicon Mac 的本地图形登录会话中执行（不支持仅 SSH、CI 或无 GUI
会话）。需要 macOS 11 或更高版本，以及 Xcode 12 / Swift 5.3 或更高版本的 Command Line
Tools；不需要预装 Python、项目包或第三方依赖。

解压源码包后执行：

```sh
./run.sh --prompt-accessibility
```

默认运行不会弹出辅助功能授权请求；上面的首次运行命令明确请求一次系统授权。

若报告为 `unsupported`，请在“系统设置 → 隐私与安全性 → 辅助功能”中确认
`AI Auto Desktop AX Runner` 已出现并开启；必要时用 `+` 选择当前目录下的
`.build/AiAutoDesktopAXRunner.app`。授权后保持源码和 `.build` 路径不变，关闭已启动的测试
窗口并重新运行：

```sh
./run.sh
```

带 prompt 的系统 API 是异步的，第一次命令返回 `unsupported` 并不代表授权操作失败。若
重编译、移动目录或删除 `.build` 后再次变成未授权，请移除系统设置里的旧条目，按上述步骤
重新授权。

`run.sh` 会在 stdout 输出单个 JSON 文档，并在 stderr 打印结果归档路径。退出码为：
`0` 通过、`1` 测试或构建失败、`3` 当前环境不支持/未授权、`64` 参数错误。默认 runner
超时为 30 秒，可用 `--timeout 1..600` 调整。watchdog 超时优先于迟到的 runner 报告，
并写入 `error.code=runner_timeout`；构建、签名、报告解析和归档阶段也使用稳定错误码及
`execution` 字段，便于远程排查。
结果目录默认位于当前目录的 `results/`，其中的 `macos-ax-test-result.tar.gz` 可直接回传。
归档只含 `report.json`、`identity.txt`、`SHA256SUMS` 和隐私说明，不含截图、屏幕像素、构建日志、用户名、主机名，
也不读取 fixture 之外的应用。
`identity.txt` 仅保存 bundle 的 designated requirement、Identifier、TeamIdentifier、
CDHash、Mach-O 架构和可执行文件 SHA-256，不保存绝对路径。
其中 Swift 版本、identity stability，以及 runner/fixture 各自的 designated requirement、
Identifier、CDHash、架构、SHA-256 都是必填证明；任何工具失败、字段缺失或空值都会让构建
失败，不会产生半份 `identity.txt`。
`report.json` 与 `identity.txt` 还会同时携带源码 revision、worktree 状态和
`source_package_digest`。digest 是规范化 `SOURCE_MANIFEST.txt` 的 SHA-256；manifest
逐项固定除自身外所有源码包成员的 mode 和 SHA-256，因此没有自引用 hash。运行前会重建并
逐字比较该 manifest，源码包被修改后会在编译和 TCC 操作前以
`invalid_source_provenance` 失败。这些值是可核对的携带数据，不会仅凭归档自述建立信任。

`run.sh` 会自动在 stderr 输出归档路径和完整 SHA-256。也可在 Mac 本机复核：

```sh
shasum -a 256 results/*/macos-ax-test-result.tar.gz
```

请回传最新的 `macos-ax-test-result.tar.gz` 和 `归档 SHA-256` 那一行；即使报告为
`failed` 或 `unsupported` 也请完整回传，以便诊断。归档内的 `SHA256SUMS` 只验证成员
自洽，不能认证回传来源；外层归档 SHA-256 也只有通过独立可信渠道取得时，才可作为
来源绑定证据。

每次运行只证明当前进程架构：Intel Mac 为 `x86_64`，Apple Silicon 原生终端为 `arm64`，
Apple Silicon 的 Rosetta 终端可能为 `x86_64`；报告同时记录 `architecture` 与
`rosetta_translated`。若资格范围要求同时覆盖两种架构，必须分别取得两份可信回传归档，
不能用单次运行替代双架构验证。

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
`SOURCE_PACKAGE_FILES.txt` 白名单内；本地验真工具由接收方仓库提供，不放入 Mac 执行包。
若未来增加真机运行所需文件，必须同步更新该白名单。

如缺少工具，先运行：

```sh
xcode-select --install
```

为减少 TCC 中的应用身份和路径变化，默认构建目录是稳定的当前目录 `.build/`；
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

接收方无需安装本项目或 Python。发布负责人应从待交付的 clean Git revision 生成源码包。
命令会输出源码 revision、worktree 状态、源码内容 SHA-256（即 package digest）和外层源码
归档 SHA-256；至少把 revision 与源码内容 SHA-256 通过独立可信渠道交给结果验真方。外层
源码归档 hash 适合传输完整性复核，但结果 verifier 绑定的是不依赖 gzip/tar 表示的内容
digest。

默认命令会拒绝白名单文件存在未提交改动。仅用于开发测试时可显式生成 dirty 包：

```sh
./package-source.sh --allow-dirty /absolute/path/dev-macos-testkit.tar.gz
```

dirty 状态会进入 manifest 和结果；即使 revision/digest 都匹配，验真器也不会令其
`source_trusted=true` 或用于资格认定。

