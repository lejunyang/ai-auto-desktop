# GTK 原生 Wayland 输入

2026-11-25 在本机私有 KWin 5.27.5、AT-SPI 2.46.0 上验证。两个 GTK 进程均报告 `GdkWaylandDisplay`，会话内未设置 `DISPLAY`。

GTK 3 插件通过目标进程现有的 AT-SPI 连接接收请求。控制端从原始观察和新观察核对节点身份、所在窗口和 scope，键盘、左键单击与滚轮都发送到目标 GTK 控件。插件随 Linux 包提供源码，编译和单应用启用命令见[模块说明](../../crates/aad-linux/gtk3-wayland-input/README.md)。

| 目标控件 | 入口 | 派发及验收点 |
|---|---|---|
| GTK 3 活动窗口中的普通可编辑 GtkEntry | `aad do key` | 插件读取 GTK 焦点控件和窗口在 ATK bridge 中的注册 ID。按下及释放前再核对焦点、映射、敏感性与密码属性。验收检查 Ctrl+A 选中原文，随后字母键替换选区。 |
| GTK 3 GtkRange、GtkDrawingArea | `aad do scroll` | 从应用读取窗口内逻辑范围；由观察到的比例计算落点，在同一窗口内逐层检查子控件分配区域。重叠分支拒绝，每格滚轮派发前重新命中。验收读取 GtkScale 的四向事件及数值变化。 |
| GTK 3 GtkButton、监听按下和释放的 GtkDrawingArea | `aad do click` | 活动窗口内无修饰键的左键单击，可指定控件内比例。公布能力前检查中心点的 GTK 控件、GDK 事件窗口和事件掩码。释放前在主循环复验原落点、接收控件、事件窗口和设备。 |
| GTK 4 的 GtkButton | `aad do invoke` | 对敏感且可见的按钮读取规范动作，派发前再次核对角色、动作名称和槽位。验收读取点击处理器记录。 |
| GTK 4 的可编辑多行文本控件 | `aad do fill` | 要求 Text 角色、MultiLine、Editable、Sensitive、Visible 和 EditableText 接口，调用目标对象的 SetTextContents。验收读取 GtkTextBuffer 的 Unicode 内容。 |

GTK 4.8.3 的适配器用 Sensitive 表示可用，子控件通常没有 Enabled 或 Showing。这里针对按钮激活和多行编辑采用该版本的语义。单行 GTK 4 输入框的遮蔽属性未必能从通用 Text 角色区分。GTK 3 插件用到的注册表、GdkEventKey 与 GdkEventScroll 属于 GTK 3 / ATK 实现。GTK 4 低层键盘、滚轮、悬停和拖动仍需独立集成。[GTK 4.8.3 状态映射](https://github.com/GNOME/gtk/blob/4.8.3/gtk/a11y/gtkatspicontext.c) · [GTK 4.8.3 角色映射](https://github.com/GNOME/gtk/blob/4.8.3/gtk/a11y/gtkatspiutils.c) · [ATK 对象注册表](https://github.com/GNOME/at-spi2-core/blob/main/atk-adaptor/accessible-register.c)

## 本机效果

* GTK 3.24.38 的按钮和绘图区分别收到左键按下、释放。按下时遮盖按钮后，目标只收到按下，调用返回 `unknown`。按钮收到 GTK 取消事件，原手势复位，按下态清除。移开遮挡后首次点击触发一次 `clicked`。在绘图区按下时关闭所属窗口也只收到按下。仅监听按下的绘图区和未加载模块的对照进程均未公布单击能力。
* GTK 3.24.38 的输入框初始没有选区。Ctrl+A 后选区为 `[0, 8]`，随后按 K 将 `INV-2037` 替换为 `k`。在按下 Z 的处理器中转移焦点，原框只收到按下阶段，邻接框未收到释放，调用报失败。非焦点输入框及已聚焦的密码框均拒绝输入。
* 同一版本的 GtkScale 在上、下、左、右各两格后，数值依次为 `50 → 70 → 50 → 30 → 50`。处理器分别记录两次对应方向。将落点移到叠加子控件覆盖的位置，滚轮事件数和值均保持不变。未加载模块的对照进程保留原文和数值，事件记录为空。
* GTK 4.8.3 的 GtkTextView 缓冲区读回 `発票 2037`，GtkButton 的点击回调执行一次。禁用按钮以及遮蔽的单行 GtkEntry 均拒绝相应动作，处理器记录和输入框内容保持不变。

滚轮命中依据限于活动窗口内的映射控件树。`GtkDrawingArea` 和独立 `GtkScrollbar` 复用同一实现，**[待验证]** 需要对应控件的真实接收器分别验收。其他 GTK 版本及 compositor **[待验证]**。

## 合成器路径

RemoteDesktop 的按键、按钮及滚动方法针对获准的会话设备，绝对移动额外指定流和流内坐标。它们的输入参数均没有目标 AT-SPI 对象和“命中此对象才派发”的条件。先读 compositor 焦点、随后发设备事件，二者之间仍可能切换窗口或同窗控件。即使用 compositor 私有扩展证明表面身份，应用内控件身份与派发时刻的命中仍需证明。因此 GTK 路径只使用应用控件或该控件自己的 AT-SPI 方法。[RemoteDesktop 协议](https://flatpak.github.io/xdg-desktop-portal/docs/doc-org.freedesktop.portal.RemoteDesktop.html) · [现有授权传输与 compositor 调研](../plan/wayland-authorized-input.md)

## 复验

在已排定的独占时隙运行。脚本启动私有 D-Bus 和独立 `kwin_wayland --virtual`，依次启动 GTK 4、GTK 3 启用模块、GTK 3 禁用模块三组新进程。需要 GTK 3 开发头文件、GTK 3/4 PyGObject、AT-SPI、KWin、C 编译器及 PowerShell 7。

```powershell
osdk exec -- cargo build --locked -p aad-cli
./tests/linux/verify-wayland-gtk.ps1 -Aad ./target/debug/aad
```
