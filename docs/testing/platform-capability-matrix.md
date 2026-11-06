# 平台能力验收矩阵

更新：2026-11-07。用户当前只提供 Linux/KDE/X11 真机；其他平台必须保留 **[待真机验收]**。
实现存在、目标 cfg 编译通过、隔离 fixture 通过、真实应用流程通过是不同证据，不互相替代。
完整变更及未完成工作见 [能力补齐计划](../plan/capability-completion-2026-11-07.md)。

| 目标环境 | 当前证据 | 仍需验收 |
| --- | --- | --- |
| 本机 KDE/X11、Qt Widgets/QML | KWrite 中文编辑保存并磁盘回读；系统设置搜索及分类导航；Dolphin 多选/框选、恢复 F2 编辑框并中文重命名后磁盘回读；Qt 密码替换/清空、键鼠长按与拖拽起点停留的独立事件计时；同 PID 双窗口拒绝错误焦点、正确窗口按下/释放回读；截图与窗口动作 | 复杂 IME；多屏和缩放；跨应用完整任务；原生会话即时取消；更多第三方应用多窗口验收 |
| 本机 Electron | Code/Codex 可恢复局部完整观察、find、scope token 刷新 | 真实点击、编辑保存和剪贴板闭环；不能把发现节点当作输入验收 |
| Chromium | 真实独立浏览器回归覆盖 DOM、frames、scope、Canvas、closed shadow host 视觉动作及内部字段保护、修饰键、敏感输入与按实例输入租约；有界面页面快捷键隔离；有界按住及焦点变化/遮挡/取消后的释放 | 路径、浏览器内部/本地页面授权；closed shadow 内部语义节点未公开；非 Chromium 不能套用这些结论 |
| Windows | UIA、SendInput、修饰键与专用保密输入有实现；目标 Windows cfg check/clippy 通过 | **[待真机验收]** Windows 交互桌面，Win32/WPF/Chromium，各种 DPI 与多屏，中文 IME，取消或超时后的键鼠释放，密码字段实际替换/清空 |
| macOS | AX 与 Swift helper 有实现；同一 fresh snapshot 复核拖拽两端；Rust macOS cfg check 及宿主逻辑测试通过 | **[待真机验收]** macOS SDK/Swift helper 构建、辅助功能与屏幕录制权限、原生/Chromium 应用、拖拽和输入效果、Secure Input 条件；Rust cfg 检查不代表 Swift 已编译 |
| GNOME/X11、XFCE/X11 | `bea9187` 已移除桌面品牌限制，保留真实会话和窗口身份检查；私有 Xvfb 四种标签真实点击/截图与拒绝路径、本机 KDE 新 helper 验收通过 | **[待真机验收]** GNOME/XFCE 窗口管理器、GTK 控件、剪贴板管理器、焦点与窗口动作；Xvfb 下更换环境标签只验证门禁和协议兼容性 |
| KDE/GNOME Wayland | 仅环境探测与 AT-SPI 语义层；XTest/根窗口截图没有 Wayland 实现 | **[待实现、待真机验收]** 获授权的 portal 输入/屏幕流、流坐标与观察元素映射、焦点和命中证明；不能把 XWayland 的 DISPLAY 当作完整 Wayland 支持 |

## 可以复现的本机检查

原生测试必须串行，浏览器测试若涉及前台或桌面租约也与原生测试串行。
测试只使用专用应用、临时文件及合成值，不依赖用户业务数据。

```powershell
# 使用同一源码构建的 aad 和 helper；绝对路径也可以。
osdk exec -- cargo build --locked -p aad-cli
pwsh -File tests/linux/verify-secret-input.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper `
  -CaptureHelper /absolute/path/to/x11_capture_helper

# Qt 应用独立计时，不以动作返回成功代替按住效果。
pwsh -File tests/linux/verify-bounded-hold.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# 同 PID 双窗口：错误 AX Focused 位不能将输入送到另一个窗口。
# helper 必须声明 window-focus；私有 Xvfb 还验证途中切换/移动的释放。
pwsh -File tests/linux/verify-x11-window-focus.ps1
pwsh -File tests/linux/verify-window-focus.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# 真实 Dolphin：只操作新建的临时文件，最终检查磁盘文件名和内容。
pwsh -File tests/linux/verify-dolphin-rename.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# AAD_TEST_CHROME 指向本机测试 Chromium；独立 profile。
osdk exec -- cargo test --locked -p aad-control --test browser_e2e -- --ignored --test-threads=1
```

密码测试由被测应用独立比较实际值，只回报相等/已清空状态；客户端同时检查输出无测试值。
真实应用验收还必须检查最终效果：编辑器回读磁盘文件，文件管理器回读选中状态或文件名，
快捷键回读目标控件和浏览器界面，不能只以 `effect=applied` 或命令退出成功判定任务完成。

## Wayland 的实现条件

应使用用户授权的 RemoteDesktop portal，会话经过 CreateSession、SelectDevices、Start；
Start 的授权结果决定允许的设备。获得会话后可用 ConnectToEIS/libei 输入，截图需要配合
ScreenCast/PipeWire。连接 EIS 后不能再混用同会话的 Notify 输入方法。
这些是接口提供的路径，尚未在本项目接成可操作后端。
参考：[RemoteDesktop portal](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html)、
[EI 协议](https://libinput.pages.freedesktop.org/libei/)。

实现时仍需把获授权流与已观察应用/元素关联，验证缩放、裁剪、显示器变更及焦点迁移；
会话撤销、锁屏、取消和断连必须停止输入并释放持有状态。不能用修改环境变量或系统级输入设备
注入替代这些证明，也不能把尚无实现的能力仅标为“待验收”。
