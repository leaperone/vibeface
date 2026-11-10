# Vibeface Desktop

`vibeface-desktop` 是 GPUI 原生客户端。`src/views` 和 `src/widgets` 绘制业务界面，`crates/client` 负责个人网络身份、加密连接、订阅和耐久 outbox，`src/host` 管理平台集成和可选本机 Node。Web 与 Mobile 独立构建。

启动窗口、扫描和选择 Node 不初始化或启动 Node。只有明确启动或安装后台节点时才改变本机运行状态。退出只关闭 App 自己持有的 Core Child；连接已有服务不取得停止权。

```sh
cargo test -p vibeface-desktop --locked
cargo clippy -p vibeface-desktop --all-targets --locked -- -D warnings
cargo run -p vibeface-desktop -- --profile-dir /absolute/task-profile --runtime-dir /absolute/sealed-runtime
node scripts/build-desktop.mjs --target <target> --runtime-dir <runtime> --runtime-archive <archive> --output <fresh-output>
node scripts/verify-desktop.mjs --target <target> --input <output>
```

图形依赖的工具链和 hosted runner 系统库由 [native-desktop action](../../.github/actions/native-desktop/action.yml) 固定。不要按 Core 的最低 Rust 版本推断 GPUI 已可编译。构建包保留 macOS ARM64 DMG/ZIP、Windows x64 当前用户安装器/ZIP、Linux x64 和 ARM64 AppImage/ZIP。封存 Core/CLI 的字节与摘要不能被桌面签名重写。

真实窗口检查使用 `scripts/e2e-desktop.mjs`，必须传入打包后的程序和同源运行包。它使用独立 profile 和显式 `--controlled-fixture`，按 PID 定位 OS 窗口，发送原生键盘输入与 Enter、保存窗口 PNG，再核对 GPUI 输入/焦点/动作回执、正常退出和无自动启动的 Node。普通 Rust 单测不证明图形、IME、无障碍、通知送达或各平台完整功能验收通过。完整验收范围见 [GPUI 设计](../../packages/docs/design/gpui-desktop.md)。

旧页面私钥不能导出为原生身份。通过本人管理设备邀请或显式恢复加入，不自动删除旧 profile、撤销旧成员或改签未确认写入。
