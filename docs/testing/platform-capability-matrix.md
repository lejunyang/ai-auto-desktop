# 平台能力验收矩阵

更新于 2026-11-27。本机 Linux/KDE/X11 的历史验收证据截至 `1733e58c`。`window-geometry-proof-v1` 的新增验收见私有 Xvfb 各行。原生 GTK 表单和 Chromium 的 GTK、KDE 文件窗口已在隔离的 KWin Wayland compositor 中运行。
实现存在、目标 cfg 编译通过、隔离 fixture 通过、真实应用流程通过是不同证据，不互相替代。
完整变更及未完成工作见 [能力补齐计划](../plan/capability-completion-2026-11-07.md)。
Electron 的两条恢复路径见 [规范动作与窗口 surface 验证](../research/electron-atspi-canonical-actions-2026-11-07.md)；截图的 scope 边界见 [完整窗口保护说明](../research/scoped-capture-window-protection.md)。

| 目标环境 | 当前证据 | 仍需验收 |
| --- | --- | --- |
| 本机 KDE/X11、Qt Widgets/QML | 截至 `1733e58c` 的历史证据：KWrite 中文编辑保存并磁盘回读；系统设置搜索及分类导航；Dolphin 多选/框选、恢复 F2 编辑框并中文重命名后磁盘回读；Qt 密码替换/清空、键鼠长按与拖拽起点停留的独立事件计时；Fcitx5 微软双拼显式物理键组合、候选截图、空格选词与 Qt/AT-SPI 提交回读；同 PID 双窗口拒绝错误焦点；同 surface 路径逐点实际轨迹、窄 scope 外 sibling overlay 和取消后的释放；双独立 Qt 进程跨资源实际 MIME 数据传递、目标可见结果及取消/移动/悬停遮挡拒绝；原生 `control_cancel` 后键鼠实际释放及下一次调用恢复；截图与窗口动作 | `window-geometry-proof-v1` 绑定后的本机 KDE/Qt 键盘、带修饰键指针、同窗与跨资源拖拽、窗口动作和 COMPOUND_TEXT 标题换算 **[待验证]**，证据截至 `1733e58c`。其他 IME、翻页和复杂组合；混合 DPI 多屏；跨应用完整任务；更多第三方应用多窗口验收；一般同步 D-Bus 调用只能在调用前后响应取消，路径专用只读 worker 与 helper 释放独立 |
| 本机 Electron | Code/Codex 可恢复局部完整观察、find、scope token 刷新；独立 VS Code profile 经原生 focus/Ctrl+A/中文 paste/Ctrl+S 后，DOM、磁盘与截图回读通过；`039352f` 规范 press 实际关闭 Code Welcome；显式完整窗口 scope 的公开截图→目视→fresh frame 比例点击也实际关闭 Welcome，独立 CDP 与 PNG 回读一致 | 精确 Close 指针目标仍可能返回 POINT_NOT_PROVEN，不能把语义 invoke 或窗口 surface 成功称为该命中修复，也不能自动扩大 scope；其他 Electron 应用、任意延迟剪贴板客户端及复杂 IME 需独立验收 |
| Chromium | 真实独立浏览器回归 34/34，覆盖 DOM、frames、scope、Canvas/closed-shadow 折线路径及内部字段保护、修饰键、敏感输入与按实例输入租约；有界面页面快捷键隔离；有界按住及异常释放；本地文件/Settings 授权与执行中祖先 URL 复核 | HTML5 自定义路径和跨资源拖拽未支持；closed shadow 内部语义节点未公开；其他内部页面逐项验收；非 Chromium 不能套用这些结论 |
| 本机 Firefox 159.0.1 headless / BiDi | 公开 CLI 验证：DOM 中文 fill、真实全选/按键/点击/滚轮、scope、PNG 截图、过期/遮挡拒绝、light/closed 密码 reveal 后持续保护、移除后截图恢复；key/click 长按实际时长、失焦/遮挡/取消释放及下一次输入；导航/刷新/标签页独立 token/visibility 与部分效果拒绝；连续同源历史与长 URL、跨域历史拒绝；独立协议/session 生命周期检查 | 子 frame 的 DOM 表单写入已验证，物理输入和截图仍需支持；双击、指针修饰键、跨域/不透明历史、drag、secret/upload/dialog 等 **[待实现]**。有界面及 Windows/macOS Firefox、复杂 IME **[待真机验收]**；当前全部输入占桌面租约，不能当作 Chromium 等价覆盖 |
| Windows | UIA、SendInput、修饰键与专用保密输入有实现；目标 Windows cfg check/clippy 通过 | **[待真机验收]** Windows 交互桌面，Win32/WPF/Chromium，各种 DPI 与多屏，中文 IME，取消或超时后的键鼠释放，密码字段实际替换/清空 |
| macOS | AX 与 Swift helper 有实现；同一 fresh snapshot 复核拖拽两端；Rust macOS cfg check 及宿主逻辑测试通过 | **[待真机验收]** macOS SDK/Swift helper 构建、辅助功能与屏幕录制权限、原生/Chromium 应用、拖拽和输入效果、Secure Input 条件；Rust cfg 检查不代表 Swift 已编译 |
| GNOME/X11、XFCE/X11 | `bea9187` 已移除桌面品牌限制，保留真实会话和窗口身份检查。私有 Xvfb 四种标签真实点击、截图和拒绝路径通过。本机 KDE helper 验收证据截至 `1733e58c` | `window-geometry-proof-v1` 绑定后的本机 KDE/Qt 键盘、带修饰键指针、同窗与跨资源拖拽、窗口动作和 COMPOUND_TEXT 标题换算 **[待验证]**，证据截至 `1733e58c`。**[待真机验收]** GNOME/XFCE 窗口管理器、GTK 控件、剪贴板管理器、焦点与窗口动作。Xvfb 下更换环境标签覆盖门禁和协议兼容性 |
| 私有 Xvfb、xfwm4 4.18、GTK 3.24.38 | 客户区及可信 WM 外框绑定。带装饰和无装饰的布局键、默认 Unicode 各组 3/3，GTK 与 AT-SPI 最终值一致。比例点击命中 Entry。带装饰直线拖放用默认和自定义图标各验证 12 段和一次放置，同进程带装饰双窗间也能经过源窗标题栏完成放置。移开图标热点后的曲线拖放两组各 5/5，逐次 12 段并放置一次。同 PID 装饰双窗经独立 EWMH 读回确认窗口动作只作用于目标。单窗口几何失配时窗口动作零派发，最小化后几何仍可对应的窗口可恢复。[GTK 复现入口](../../tests/linux/verify-x11-decorated-input.py) | **[待验证]** GTK 默认拖拽图标遮挡中的显式曲线路径、其他 WM、带装饰的 Qt 和混合 DPI 真机。 |
| 本机 Qt/X11 应用缩放 | 截至 `1733e58c` 的历史证据：`597656c` 按准确窗口身份校准逻辑/物理坐标；单显示器 Qt 1/1.5/2 倍应用缩放下的实际点击、F2 与局部 PNG；歧义和 scope 拒绝；8 项标题编码检查；Dolphin 中文目录重命名与 KWrite 中文文件保存经磁盘回读通过 | `window-geometry-proof-v1` 绑定后的本机 KDE/Qt 键盘、带修饰键指针、同窗与跨资源拖拽、窗口动作和 COMPOUND_TEXT 标题换算 **[待验证]**，证据截至 `1733e58c`。**[待真机验收]** 混合 DPI 多屏、旋转与热插拔。非 Qt 应用和 Wayland 的缩放需独立验收 |
| 本机 Qt/X11 局部截图 | `2e00547` 私下检查完整所属窗口；4/4 公开入口检查，无覆盖及非重叠密码框时 PNG 像素与尺寸正确，scope 外普通/密码控件覆盖时拒绝；原 scope 继续有效 | 未把私有检查升级为外部节点操作授权；无法完整观察所属窗口时仍拒绝截图；不代表所有第三方自绘界面都能证明无遮挡 |
| 私有 Xvfb、bspwm 0.9.10、Qt 5.15.8 | `window-geometry-proof-v1` 的按键、激活、同窗与跨资源拖拽使用空 `frames` 绑定。焦点 3/3，分段拖拽 7/7，跨进程拖放 4/4。应用缩放 5/5，在 1/1.5/2 倍核对控件点击、F2 文本和 PNG 尺寸。局部截图 scope 4/4。[Qt 复现入口](../architecture/linux-kde-x11-driver.md#验证) | **[待验证]** 本机 KWin 装饰窗口、带修饰键指针、COMPOUND_TEXT 标题换算、混合 DPI 真机 |
| 原生 KWin Wayland 与 GTK 3 | 独立 compositor、关闭 X11 显示连接，经公开 CLI 枚举应用、填写发票号、触发保存。应用回执同时记录了发票号和 GdkWaylandDisplay。[复现入口](../../tests/linux/verify-wayland-form.ps1) | 其他应用的控件语义、物理键鼠、多屏和授权撤销行为需分别验收。 |
| KWin Wayland GTK 定向输入 | GTK 3.24.38 的应用模块支持编辑框按键、四向滚轮，以及按钮和绘图区的左键单击。已核对全选后替换、滑块数值和实际鼠标事件。按钮按下后被遮挡时会取消该手势，移开遮挡后再次点击只触发一次。GTK 4.8.3 通过控件接口激活按钮、填写多行文本。[控件与模块](../research/gtk-wayland-input.md) | GTK 4 的原始按键与鼠标事件、任意 GTK 控件的拖放。 |
| KWin Wayland Electron 工作台 | 用 CDP 选定 VS Code 1.146.0 的工作台文档，真实指针点击搜索侧栏，搜索框收到了 Ctrl+A 和退格，选区和内容均有独立回读。[启动和授权](../browser-script.md#在-wayland-上操作-electron-应用窗口) | 未开放 CDP 的应用需依赖其无障碍控件。Electron 原生菜单和跨应用拖放需桌面端支持。 |
| KWin Wayland Electron 原生 AT-SPI | Electron 43.8.6 独立应用通过已观察链接的 `jump` 跳转至页内锚点，复选框的 `check` 和 `uncheck` 各产生一次目标事件，旧观察被拒绝。[验收步骤和环境](../research/electron-atspi-canonical-actions-2026-11-07.md#wayland-上的链接与复选框) | 其他运行时版本、自绘控件和原生菜单。 |
| KWin Wayland 文件窗口 | 选择网页附件、读取原生文件句柄、另存为新文件、选择已有目录。GTK 和 KDE 均核对了实际内容或系统返回的目录 URI。[命令与窗口条件](../browser-recording.md#在已经打开的文件选择器里选文件) | 更多桌面和本地化窗口。 |
| Wayland 屏幕共享与输入 | `aad assist --desktop --view-only` 可请求共享一块显示器。已验证 PipeWire 流选择、撤销和真实 KWin 共享窗口的取消。Qt 5.15.8 和 Qt 6.4.2 的 Widgets 插件可向已确认的控件发按键和指针事件，已验证同进程跨控件及跨窗口复制文本和标准列表项。[插件用法与范围](../../crates/aad-linux/qt-wayland-input/README.md) | **[待验证]** 操作员在真实共享窗口同意后，从查看端核对画面。跨进程拖放和 compositor 级命中校验。[授权输入路线](../plan/wayland-authorized-input.md) |

## 可以复现的本机检查

原生测试必须串行，浏览器测试若涉及前台或桌面租约也与原生测试串行。
测试只使用专用应用、临时文件及合成值，不依赖用户业务数据。

```powershell
# 使用同一源码构建的 aad 和 helper；绝对路径也可以。
osdk exec -- cargo build --locked -p aad-cli
pwsh -File tests/linux/verify-native-scale.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper `
  -CaptureHelper /absolute/path/to/x11_capture_helper
pwsh -File tests/linux/verify-x11-window-title.ps1
pwsh -File tests/linux/verify-dolphin-rename.ps1 `
  -Aad target/debug/aad -InputHelper /absolute/path/to/x11_xtest_helper -UnicodeWindowTitle
pwsh -File tests/linux/verify-secret-input.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper `
  -CaptureHelper /absolute/path/to/x11_capture_helper

# 局部截图仍检查完整窗口中覆盖目标的兄弟控件，包括保护字段。
pwsh -File tests/linux/verify-scoped-capture.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper `
  -CaptureHelper /absolute/path/to/x11_capture_helper

# Qt 应用独立计时，不以动作返回成功代替按住效果。
pwsh -File tests/linux/verify-bounded-hold.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# 应用确认 down 后才发送 control_cancel；独立回读 early up 与下一次调用。
# helper 必须声明 cooperative-cancel。Xvfb 另验证长文本/粘贴/全局测试组合键。
pwsh -File tests/linux/verify-x11-cancel.ps1
pwsh -File tests/linux/verify-native-cancel.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# 同 PID 双窗口：错误 AX Focused 位不能将输入送到另一个窗口。
# helper 必须声明 window-focus；私有 Xvfb 还验证途中切换/移动的释放。
pwsh -File tests/linux/verify-x11-window-focus.ps1
pwsh -File tests/linux/verify-window-focus.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# 路径 ACK 协议与真实 Qt 逐点事件；覆盖窄 scope 外的同窗口 sibling overlay。
# helper 必须声明 anchored-drag-path。测试报告同时记录耗时。
pwsh -File tests/linux/verify-x11-drag-path.ps1
pwsh -File tests/linux/verify-native-drag-path.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# 跨资源：私有双 PID/WM/覆盖协议 + 真 Qt 两进程数据传递与释放。
# helper 必须声明 anchored-drag-between；全程只操作新建 fixture。
pwsh -File tests/linux/verify-x11-cross-drag.ps1
pwsh -File tests/linux/verify-native-cross-drag.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# 真实 Dolphin：只操作新建的临时文件，最终检查磁盘文件名和内容。
pwsh -File tests/linux/verify-dolphin-rename.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# 剪贴板多次取数、后续复制、请求洪泛的私有 Xvfb 协议回归。
# 真实 Code 使用新 profile 和临时文件；Node 的 CDP 连接只作独立回读。
pwsh -File tests/linux/verify-x11-paste-sequence.ps1
pwsh -File tests/linux/verify-code-paste.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# Code 的空本地化标签不阻止规范 press；Qt Press 同时做回归。
pwsh -File tests/linux/verify-canonical-invoke.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper

# 显式 keys 模式只使用现有 XKB 键位，helper 声明 type-text-keys。
# 此例对应本机已配置的 Fcitx5 微软双拼；测试不改输入法列表，并恢复原方法。
pwsh -File tests/linux/verify-x11-physical-keys.ps1
pwsh -File tests/linux/verify-fcitx-physical-keys.ps1 `
  -Aad target/debug/aad `
  -InputHelper /absolute/path/to/x11_xtest_helper `
  -ComposingMethod shuangpin -Keystrokes nihk -ExpectedText '你好'

# AAD_TEST_CHROME 指向本机测试 Chromium；独立 profile。
osdk exec -- cargo test --locked -p aad-control --test browser_e2e -- --ignored --test-threads=1

# Linux + Node >=22；脚本启动独立 Firefox headless profile，所有写操作经公开 CLI。
# BiDi 当前保守占用 desktop-input，仍须与其他输入测试串行。
$env:AAD_TEST_FIREFOX = '/absolute/path/to/firefox'
$env:AAD_TEST_AAD = (Resolve-Path target/debug/aad).Path
node tests/browser/firefox_bidi_e2e.mjs
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
