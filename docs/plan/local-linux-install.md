# Linux 源码包交接

构建入口（PowerShell 7，干净的已提交工作区）：

```powershell
./scripts/release/build-local-linux.ps1
```

需要 osdk 管理的 Rust 1.98.0、g++、pkg-config、X11/XTest 开发库。
脚本使用项目工具链，构建 aad、aad-browser 和两个 Linux helper，复用发布打包脚本。
打包脚本解包后执行 CLI 和 browser helper 的版本/帮助检查。
dist 内的 `.sha256` 校验压缩包，`.source.json` 记录源码 commit、架构、编译器及包 hash。
这不是字节级可复现构建承诺；发布时须一起保留包和来源清单。

负责人安装前需完成相关 crate tests、clippy、隔离浏览器 fixture 和独立评审。
浏览器 fixture 使用 loopback、临时 profile；缺浏览器时先准备测试浏览器。
源码包构建成功不等于 GUI 或二维码验收通过。

安装步骤（评审通过后由负责人执行）：

1. 核对 source.json 的 commit 与批准提交一致，执行 `sha256sum -c <包名>.sha256`。
2. 将包解到新的版本目录，保留四个可执行文件的相邻关系与执行权限。
   不覆盖当前安装。记录旧 `command -v aad`、`readlink -f` 和 `osdk list` 输出。
3. 先用新目录的绝对路径运行 `aad --version`、`aad probe`，以及真实 MCP/浏览器 fixture。
   任何检查失败都保留旧安装并停止切换。
4. 将新版本目录加入实际启动 omem 的环境 PATH 首位。新 shell 中核对
   `command -v aad`、`readlink -f`、`aad --version` 和二进制 SHA256。
   通过 omem 实际调用检查其解析路径；仅新 shell 正确还不算完成。
5. 在新解析链验证后，用 `osdk uninstall --help` 核实当前版本参数，逐一卸载
   `osdk list` 中的 `github:lejunyang/ai-auto-desktop` 发布版本。
   不删除其他工具或手工修改 osdk 目录。再次核对 osdk list、新 shell 和 omem。

回滚：卸载旧版之前，恢复原 PATH 即可；已卸载时用 osdk 官方 install/use
恢复记录的原版本，再恢复 omem 的启动环境并验证实际解析。
保留新包与失败日志用于排查，不删除用户 profile，不关闭用户浏览器。
