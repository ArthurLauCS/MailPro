# MailPro

**看信，不让发件人知道。**

[English](README.md) | 简体中文

注重隐私的 Windows 桌面邮件客户端，Rust 编写，界面用原生 GPU 渲染（gpui）。默认不加载任何远程内容，发件人无法通过邮件得知你何时、在哪里打开了它。每个邮箱经各自的 SOCKS5 代理连接，代理连不上直接报错，绝不退回直连。

## 功能

1. **默认挡住跟踪。** 邮件 HTML 经白名单清洗：脚本、框架、表单和远程资源删掉或换成本地占位地址。正文在 WebView2 里显示，关闭脚本，内容安全策略为 `default-src 'none'`，它发出的每个请求都由程序在本地应答，内核自己不联网。
2. **远程图片只在你允许时加载**：只这一封，或总是加载某个发件人。图片由 MailPro 经该邮箱的代理代取，不带 cookie 和 Referer，User-Agent 用通用值；字体、样式表、音视频照样拦下。图片不缓存：每加载一次，发件人都会再看到一次（看到的是代理的 IP）。
3. **看清谁在跟踪。** 阅读区头部列出拦下和已加载的远程资源的域名及次数。
4. **每个邮箱单独设代理。** SOCKS5 `主机:端口`；域名交给代理解析，邮件服务器的域名不会出现在本机 DNS 里。代理连不上时连接直接失败，不会改走直连。没设代理的邮箱在设置里标“直连”。
5. **安全打开链接。** 点链接先显示清洗后的地址：拆掉已知的跳转壳（Google、Outlook SafeLinks、Facebook 等），去掉跟踪参数，原地址另列一行。可以用系统默认浏览器、自选程序或 [AdsPower](https://www.adspower.com/) 环境打开；AdsPower 环境经它的本地 API 启动，指纹和代理照常生效。
6. **发信少露信息。** 纯文本邮件经该邮箱的代理、走 TLS 发出。SMTP 握手报 `[127.0.0.1]` 而不是电脑名，Date 用 UTC 不暴露时区，Message-ID 不含主机名，也不带邮件程序的名字。
7. **Gmail、Outlook 和其他 IMAP 邮箱。** Gmail 用应用专用密码登录；Outlook / Hotmail 在浏览器里的微软登录页登录（OAuth，不存密码）；其他邮箱手填 IMAP 服务器，只走 TLS。已读状态存在服务器上，多台电脑自动一致：本地标已读几秒内写回；别处的改动几秒内推到收件箱（IDLE），其他文件夹 5 分钟内跟上。
8. **大文件夹也流畅。** 全部文件夹的邮件头全量同步，存在本地 SQLite 里；正文打开时才下载。5 万封的文件夹滚动时每帧耗时中位 2.8 ms。
9. **无遥测、无崩溃上报。** 界面中英双语，深浅色跟随系统，深色时邮件正文也变深。

这些说法的验证：一封含 31 种跟踪手段的测试邮件，在无防护的 headless Chrome 里 31 种全部触发，在 MailPro 里一种都没有；[emailprivacytester.com](https://www.emailprivacytester.com/) 的测试在加载图片前没有任何新记录，加载后也只多出代理的 IP；使用十分钟内 WebView2 进程没有任何对外连接。

## 下载安装

**[下载安装包](https://github.com/ArthurLauCS/MailPro/releases/download/v0.1.0/MailPro-Setup-0.1.0.exe)** · [SHA256 校验值](https://github.com/ArthurLauCS/MailPro/releases/download/v0.1.0/SHA256SUMS.txt)

从 [GitHub Releases](https://github.com/ArthurLauCS/MailPro/releases/tag/v0.1.0) 下载 `MailPro-Setup-0.1.0.exe` 运行即可。按当前用户安装到 `%LOCALAPPDATA%\Programs\MailPro`，不需要管理员权限。

- Windows 10 / 11 x64，需要 [WebView2 Runtime](https://developer.microsoft.com/microsoft-edge/webview2/)（Windows 11 和保持更新的 Windows 10 已自带）。
- 安装程序提供中文和英文；软件界面语言在软件里另选。
- 安装包没有签名，Windows SmartScreen 可能提示未知发布者：点“更多信息 → 仍要运行”。可用 `Get-FileHash .\MailPro-Setup-0.1.0.exe -Algorithm SHA256` 对照 `SHA256SUMS.txt` 校验。

邮件和设置存在 `%LOCALAPPDATA%\MailPro`，密码和 Outlook 的登录令牌存在 Windows 凭据管理器。卸载时两者一并删除。

## 使用

1. 左栏底部 **添加账号**：填邮箱、应用专用密码，建议再填 SOCKS5 代理，如 `127.0.0.1:7890`。Gmail 需先开启两步验证，再到 [myaccount.google.com/apppasswords](https://myaccount.google.com/apppasswords) 创建应用专用密码。Outlook / Hotmail 先在 Outlook 网页版 设置 → 邮件 → 转发和 IMAP 里打开 IMAP；再填邮箱和代理，点 **登录**：微软登录页在 **浏览器**（第 3 条）选定的浏览器里打开，用的是那个浏览器的网络；选同一个账号并允许访问。每个 Outlook 邮箱各自登录。以后授权被撤销或失效时，账号下会提示，在账号设置里点 **重新登录**。其他邮箱还要填 IMAP 服务器和端口（只支持 TLS），以及 SMTP 服务器和端口（587 端口走 STARTTLS，其他端口走 SSL/TLS，一般是 465）。SMTP 服务器留空时按 IMAP 服务器把 `imap.` 换成 `smtp.`。
2. 打开邮件即在服务器上标为已读。阅读区头部列出拦下的远程资源；**加载图片** 只对这一封生效，**总是加载此发件人的图片** 对该地址的所有邮件生效。
3. 左栏底部 **浏览器** 选择链接用什么打开。选 AdsPower 时填环境 ID 和 AdsPower API 设置里的 API key；key 存在凭据管理器里。
4. 把账号行拖到另一个账号上即可调整顺序。收件箱最近 30 分钟内收到过邮件时，账号行会微微泛光。
5. 点邮件即打开；Ctrl 点增减一封，Shift 点选中一段。阅读区顶部的按钮或右键菜单可以归档、删除、移动、标已读 / 未读、加星；列表顶部的按钮把整个文件夹标为已读。删除是移到“已删除”，在“已删除”里再删除会先确认，然后彻底删除。改动都写回服务器；移动和删除要账号连着才能做。
6. 列表顶部的搜索框找主题或发件人里含有所输入每个词的邮件。旁边的按钮在 **当前文件夹** 和 **全部邮箱**（所有账号的所有文件夹，结果标出所在文件夹）之间切换。不搜正文。点文件夹即清空搜索。
7. 列表顶部的笔形按钮写新邮件；阅读区头部有 **回复**、**全部回复**、**转发**，正文里的 mailto: 链接也直接写信。关掉写信框时内容改过就存进草稿，点草稿接着写。发出的信存一份进已发送（Gmail 自己存）。发信名字在账号设置里填，可不填。
8. 关掉窗口后 MailPro 留在通知区域继续收信：点它的图标唤出窗口，右键选 **退出** 才真正退出；再次启动 MailPro 也会唤出原来的窗口。窗口不在前台时，收件箱来了新的未读信会在桌面右下角弹通知：一封显示发件人和主题，点开即打开它；同时到多封显示该邮箱的封数，点开打开收件箱。不想要通知时在 Windows 设置 → 系统 → 通知里关掉。

## 当前限制

尚未支持：写信时加附件和格式，设置导入 / 导出。Outlook / Hotmail 只支持个人微软账户，不支持工作或学校账户。

## 开发

需要 Rust stable 和 MSVC C++ 工具；打安装包需要 [Inno Setup 6](https://jrsoftware.org/isinfo.php)。

```powershell
.\scripts\fonts.ps1                           # 下载打包的两套字体（不进 git）；没有时界面用系统字体
cargo run -p mailpro                          # 运行软件
cargo run -p mailpro -- C:\path\to\message.eml  # 只显示一封邮件文件
cargo test --workspace
cargo run -p mailpro-core --bin mailpro-cli   # 管理账号、同步的命令行工具，用法见源码开头
.\scripts\installer.ps1                       # 安装包：target\installer\MailPro-Setup-<版本>.exe
```

- 隐私检查：`python tests/privacy/tracker.py serve <log>` 启动本地计数服务器，在 MailPro 里打开 `tests/privacy/tracking.eml`（`scripts/drive.ps1` 可在不抢焦点的情况下运行），日志必须为空。
- 对本地 GreenMail 服务器的 IMAP 同步测试：见 `crates/core/tests/sync.rs` 开头。
- 应用图标来自 `crates/app/assets/icon.svg` 和 `icon-small.svg`，改完运行 `python scripts/icon.py`。依赖变化后运行 `python scripts/notices.py`。

| 路径 | 职责 |
| --- | --- |
| `crates/core/` | 不依赖界面的邮件逻辑：解析、HTML 清洗、链接清洗、IMAP、经代理的 HTTP、SQLite 存储、凭据、同步引擎 |
| `crates/app/` | gpui 界面和 WebView2 正文查看器 |
| `tests/privacy/` | 跟踪测试邮件和本地计数服务器 |
| `installer/` | Inno Setup 脚本 |
| `docs/PLAN.md` | 计划、决策和进度 |

## 反馈与许可

问题请提交到 [Issues](https://github.com/ArthurLauCS/MailPro/issues)，附上版本、Windows 版本和复现步骤。分享日志前删掉邮箱地址和邮件内容。

第三方许可声明见 [THIRD_PARTY_NOTICES.txt](THIRD_PARTY_NOTICES.txt)，安装目录里也有一份。MailPro 采用 [MIT 许可证](LICENSE)。
