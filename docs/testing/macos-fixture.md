# macOS Accessibility 真机 fixture 测试

仓库提供 `tests/macos/` 自包含测试套件，供用户在真实 Intel 或 Apple Silicon Mac 的
可交互图形登录会话中执行；纯 SSH、CI 或无 GUI 会话不在支持范围内。详细命令和授权步骤
见 `tests/macos/README.md`。源码包不依赖已安装的本项目或 Python，只需要 macOS 11+ 与
Xcode 12 / Swift 5.3+ Command Line Tools。

测试通过系统 `xcrun`/`swiftc` 构建固定 bundle ID、ad-hoc 签名的 AppKit fixture 和
AX runner。授权可用时，runner 只从 fixture PID 创建 `AXUIElement`，进行有界遍历与唯一
定位，然后验证 `AXFocused`、`AXValue`、`AXPress`；每个动作的成功都必须由新一次 AX
读取确认，不能只依赖写 API 的返回码。套件也按 `type_text` 的显式 CGEvent 路径分段覆盖
ASCII、中文和非 BMP emoji；每段输入前重新解析 AX 目标并确认 focus/前台，输入后以 fresh
snapshot 验证累计值。每段还在首事件前记录 `IsSecureEventInputEnabled()`，启用时 fail closed。
`NSSecureTextField` 负向 case 在任何事件发布前拒绝 secure text；报告只声明事件已提交，不把
void `postToPid` 当成接收确认。
显式 `pointer_click` 使用 fixture 中独立的按钮和状态文本。runner 在动作前从自有 fixture PID
的 fresh AX snapshot 按 identifier 唯一定位按钮，要求 `AXPosition`/`AXSize` 可读且 bounds 面积
为正，只由 bounds 计算中心点，并校验 element PID 与 fixture PID、事件创建前和实际提交前确认 fixture
frontmost；中心点 AX hit test 还必须解析回同一按钮，且独立状态必须仍为 idle。随后它向
fixture PID 定向提交 `.mouseMoved`、`.leftMouseDown`、`.leftMouseUp`
CGEvent，动作后从 fresh AX snapshot 读取状态作为唯一成功判据。该路径不接受裸坐标，不截图、
不做 OCR，也不使用全局 event tap。
每个被访问的 AX element 还会设置消息超时，避免
目标进程无响应时无限等待。
runner 由 LaunchServices 以固定 `.app` 身份启动，并将报告原子写入结果目录；外层脚本
根据报告中的状态归一退出码，而不把 `open` 的状态误当成 AX 测试结果。

安全和权限边界如下：

- 默认只调用 `AXIsProcessTrusted()`，不会静默弹窗；只有用户显式传入
  `--prompt-accessibility` 才调用带 prompt 的 trust check。
- 调用 `CGPreflightScreenCaptureAccess()` 只读取状态；不请求屏幕录制授权，也不截图。
- AX 树限定于测试套件自行启动的 fixture 进程，默认最大深度 8、最多 128 个节点。
- 可回传归档只包含结构化 JSON、签名/架构证明、SHA-256 清单和隐私说明，不含屏幕内容或其他应用数据；签名证明不保存绝对路径。Swift 编译失败时，隐私说明末尾可携带有界脱敏诊断（最多 120 行、每行 512 bytes、正文 12 KiB），绝对路径和非打印字符会被移除，且不会新增归档成员。
- 结果归档会把 owner/group 固定为 root/0、mtime 固定为 2000-01-01 UTC，并把普通文件
  mode 固定为 `0644`；同时关闭 ACL、file flags、xattr、AppleDouble 和 gzip header 时间/文件名。
  它只接受已知的 macOS bsdtar/libarchive 或 GNU tar，归一化能力不可用时会 fail closed。
- `identity.txt` 强制包含 Swift 版本、identity stability，以及 runner/fixture 各自非空的
  designated requirement、requirement origin（`implicit`/`explicit`）、Identifier、CDHash、architectures 和 SHA-256；采集命令失败不会被
  `sed`/`awk` 等管道末端掩盖，也不会发布半份证明。
- designated requirement 只接受 `codesign -d -r-` 唯一一条规范的 explicit
  `designated => expression` 或 implicit `# designated => expression`，随后以
  `codesign --verify --strict --test-requirement "=expression"` 回验对应 app；缺失、重复、
  空值、畸形或回验失败都会以稳定阶段码 fail closed。ad-hoc（`ephemeral`）构建必须是
  implicit；固定签名身份可为 explicit 或 implicit。新的 `passed` 归档必须携带 origin，
  缺失该字段的旧 `passed` 归档不再被当前 verifier 接受。
- 源码包注入规范化 `SOURCE_MANIFEST.txt`：包含 Git commit SHA、clean/dirty 状态，并固定
  除自身外每个白名单成员的 mode 和 SHA-256。`source_package_digest` 是该 manifest 的
  SHA-256，以此避开自引用 hash；Mac 在构建前重算并验证，随后将 revision、worktree 与
  digest 同时写入 report 和 identity。
- 缺少 macOS、Command Line Tools、受支持 Swift 版本、原生架构或授权时输出结构化
  `unsupported`，不会伪造通过。watchdog 超时及 launcher/build/archive 失败会写入稳定的
  `error.code` 和有界的 `execution` 诊断字段；一旦 watchdog 判定超时，不接受迟到的
  `passed` 报告。

固定 bundle ID 与稳定构建路径可以减少 TCC 身份漂移，但 ad-hoc 签名在重编译后不保证
保留授权，其身份稳定性只能标为 `ephemeral`；长期固定 runner 应通过
`MACOS_TEST_CODESIGN_IDENTITY` 使用同一 Developer ID 签名。带 prompt 的 AX API 是异步的，
本次运行仍以普通 `AXIsProcessTrusted()` 的即时结果为准。
每次构建和运行只覆盖当前 `uname -m` 架构；报告同时记录 `architecture` 与 Rosetta 转译
状态。需要 arm64 与 x86_64 双架构资格时，必须分别收集并验真两份归档。

这是一条真机 fixture 验证链路，不等同于对任意第三方应用、锁屏、安全输入、跨用户会话
或完整 macOS 平台兼容性的资格声明。Linux 机器只能对 shell 和源码结构进行静态检查。
真机 kit 已实现 `desktop.macos_ax.type_text@1` 对应的 Unicode/secure text case，以及
`desktop.macos_ax.pointer_click@1` 对应的 fixture 中心左键 case；在真实 Mac 生成 `passed` 报告
前，仍不能把它写成已经通过资格验证。验证只需 Accessibility，不请求 Screen Recording；
pointer click 是显式动作，不作为 `set_value`、`invoke` 或其他动作的自动 fallback。fixture、runner 与说明均已纳入
`tests/macos/SOURCE_PACKAGE_FILES.txt` 白名单。
passed 报告还必须包含并通过 `pointer_click_and_reread`；本地 verifier 缺少该 check 时以
`missing_required_checks` fail closed。新增 case 不改变 source provenance：report/identity 仍须
一致携带 revision、worktree 与 package digest，资格认定仍要求独立可信的归档 hash 与 clean
source pins。

需要把真机套件交给仓库外的 Mac 时，在 `tests/macos/` 执行：

```sh
./package-source.sh /absolute/path/macos-ax-testkit-source.tar.gz
```

该命令按 `SOURCE_PACKAGE_FILES.txt` 白名单生成平铺的规范化源码包，不包含构建结果。默认
要求白名单文件对应 clean Git worktree；开发中的未提交快照只能显式使用 `--allow-dirty`，
且这样的结果永不获得 `source_trusted`。接收方
解压到空目录后运行 `./run.sh --prompt-accessibility`，在系统设置中给
`AI Auto Desktop AX Runner` 开启辅助功能后再运行 `./run.sh`。脚本会打印结果归档路径和
外层 SHA-256；无论 `passed`、`failed` 还是 `unsupported`，都应回传该归档和 hash 行。最终
对外交付源码包应由发布负责人从待交付 revision 生成，并通过可信渠道附上命令打印的
`源码 revision` 与 `源码内容 SHA-256`。后者是 manifest 内容摘要，不是会受 tar/gzip 表示
影响的外层源码归档 SHA-256。

## 在本地验真回传归档

收到 `macos-ax-test-result.tar.gz` 后，不要直接解压或据文件名人工判断。请在本仓库执行：

```sh
tests/macos/verify-result.sh /absolute/path/macos-ax-test-result.tar.gz
```

验真器不向磁盘提取任何成员，并对压缩输入、解压后的 tar、单个成员和成员总量设置硬上限。
它只接受 `run.sh` 生成的四个平铺普通文件，先拒绝路径穿越、重复/额外成员、符号链接、
硬链接、设备文件及其他特殊类型，再校验 `SHA256SUMS`、报告 schema、状态、checks/summary，
identity 的固定 bundle ID、架构和 hash 字段，以及生成器承诺的 gzip 单一 member、无名称、
零时间、固定 XFL/OS 和 tar 成员顺序、mode、uid/gid、uname/gname、mtime 等归一化元数据。

stdout 始终只有一个 JSON 文档。`archive_valid`（兼容别名 `verified_archive`）只表示归档
结构、内容 hash 和报告格式自洽；`report_passed` 只表示归档内的报告自称通过，两者都不认证
回传方或真实 Mac 来源。当前 testkit 没有预置签名、公钥或挑战，因此默认调用即使收到自洽
的 `passed` 报告，也返回 `trusted_archive=false`、`qualified=false` 和非零退出码。

若测试请求方已经通过与归档回传通道独立的可信渠道取得完整归档 SHA-256，可执行：

```sh
tests/macos/verify-result.sh \
  --expected-archive-sha256 <independently-trusted-64-hex> \
  --expected-source-revision <independently-trusted-git-commit-sha> \
  --expected-source-package-digest <independently-trusted-64-hex> \
  /absolute/path/macos-ax-test-result.tar.gz
```

只有外层结果归档 hash 匹配，才设置 `trusted_archive=true`；只有独立可信的源码 revision 与
package digest 都匹配、report/identity 一致且 `source_worktree=clean`，才设置
`source_trusted=true`。报告还必须为 `passed`，三者同时成立才返回 `qualified=true` 和退出码
`0`。外层归档 hash 的既有信任语义保持不变，源码 pin 不会替代结果归档 pin；归档内自述的
任意 hash 也不能自证来源。把预期值和归档由同一回传方、同一消息或同一不可信位置一起提供，
不能建立信任。旧报告仍可做结构与内容验真，但缺少 source provenance 时即使提供新参数也会
以 `source_provenance_missing` fail closed。完整的 `failed`/`unsupported` 报告仍会返回 `archive_valid=true`、
`report_passed=false`、`qualified=false`；结构、hash 或语义失败则 `archive_valid=false`。
