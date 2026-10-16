# 构建与发布

[CI](https://github.com/leaperone/vibeface/actions/workflows/ci.yml) 在 PR、`main` push 和 `v*` tag 上运行同一流程。PR 与 `main` 只上传 Actions 制品，不创建公开版本，也不部署主机。公开版本在 [Releases](https://github.com/leaperone/vibeface/releases) 下载。

## 可下载的包

每个平台的 Actions artifact 包含 `vibeface-node-<version>-<target>.tar.gz` 和同名 `.sha256`。四种包均在对应架构的原生 runner 构建，解包后运行 Node 多进程验收，不调用模型。

| 平台 | Rust target | 构建和验收环境 |
| --- | --- | --- |
| Linux x64 | `x86_64-unknown-linux-gnu` | Ubuntu 24.04 |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | Ubuntu 24.04 ARM64 |
| macOS Intel | `x86_64-apple-darwin` | macOS 15 Intel |
| macOS Apple Silicon | `aarch64-apple-darwin` | macOS 15 ARM64 |

Linux 包依赖 glibc，未在旧版发行版验收。macOS 包没有 Apple 开发者签名或 notarization。根据设备选择包，在存放包的目录先校验再解压：

```sh
shasum -a 256 -c vibeface-node-<version>-<target>.tar.gz.sha256
tar -xzf vibeface-node-<version>-<target>.tar.gz
cd vibeface-node-<version>-<target>
./vibeface-node --version
./vibeface-node --help
```

包内有 Rust 程序、SDK sidecar 源码、文档、用户级服务安装脚本和 `BUILD-INFO.json`，不含身份、配置、数据库、Provider 程序、模型凭证或 npm 依赖。干净 Git 提交的正常构建会记录版本、target 和源码提交；`--skip-build` 或未提交源码构建的 `source_commit` 为 `null`，不能作为提交身份的证明。

运行任务前须另装并登录相应 Provider。Codex SDK 管理路径还需 Node.js 18+，在包的 `sidecars/` 下执行 `npm ci --omit=dev`。Cursor 管理路径需另装 Cursor SDK Bridge 并配置凭证。初始化、配对、服务安装和公网 TLS 见 [Node 使用说明](node.md)。

## CI 验证范围

质量检查包含 Rust fmt、workspace tests、Clippy、声明的最低 Rust 版本编译检查、工作流和 shell 静态检查、JavaScript 语法，以及 Atlas 迁移文件校验；不连接用户数据库。打包任务校验 SHA256、源码提交、版本和 target，再启动解包后的程序运行现有多进程验收。Linux x64 还安装包内锁定的 SDK 依赖并导入模块，不开启模型会话。

验收失败时只上传报告和三个测试进程日志，不上传测试目录或数据库。正常制品保留 14 天，诊断保留 7 天。CI 不验证真实模型调用、实体手机、物理局域网、公网反向代理或用户后台服务安装。

## 显式发布版本

1. 通过 PR 更新 workspace 版本及 `Cargo.lock`，完成检查并合并到 `main`。
2. 在已合并的提交创建并推送 `v<version>` tag。tag 必须与 Cargo 版本完全相同，且指向已进入 `main` 的提交。
3. 查看 tag 触发的 CI，确认质量检查和四个平台的包验收全部成功。
4. 发布任务创建 draft Release，上传四个包和四个 checksum，核对资产清单并重新下载逐字节比较，最后公开 Release。

只有发布任务拥有 `contents: write` 权限。失败留下的 draft 可以在修复原因后重跑；公开 Release 不会被覆盖。同名公开版本需要更正时，使用新的版本和 tag。合并 PR 不会自动推送 tag。

## 更换本机程序

先停止 Node 或卸载用户服务，确认进程已经退出，再备份完整数据目录；目录包含身份和历史，备份应保持私密。校验并解压新包到单独目录，保留原数据目录，用新程序前台启动检查。确认可用后重新安装指向新路径的用户服务。不要让新旧程序同时使用同一数据目录。

没有自动更新或旧版数据兼容承诺。若新版本改变存储格式，应按该版本说明处理；不要直接让旧程序打开已经升级的数据。需要恢复时，停止程序并恢复对应版本的完整备份。
