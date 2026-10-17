# 安装与管理节点

Vibeface 的主程序是 Rust 二进制 CLI `vibeface`，网页随程序内嵌分发。支持 macOS Apple Silicon、Linux x64 和 Linux ARM64；Linux 包需要 glibc。macOS 包尚无 Apple 开发者签名或 notarization。

## 选择安装渠道

首次安装最新正式版，默认渠道是 Stable：

```sh
curl -fsSL https://raw.githubusercontent.com/leaperone/vibeface/main/install.sh | bash
```

当前尚无 Stable Release，使用上述命令会明确失败。要安装开发预览版，请显式选择 Nightly：

```sh
curl -fsSL https://raw.githubusercontent.com/leaperone/vibeface/main/install.sh | bash -s -- --channel nightly
```

安装器依赖系统 Bash、curl、tar、gzip 和 SHA256 工具，不需要 Python、jq、Rust 或 Node.js。它按数值版本选择已公开的渠道版本，不选择 rc，也不会因最新包缺少当前平台而退回旧版。网络或 GitHub API 限流失败会保留错误。

如需先检查脚本，再运行：

```sh
curl -fsSL https://raw.githubusercontent.com/leaperone/vibeface/main/install.sh -o install.sh
bash install.sh --help
bash install.sh --channel nightly
```

安装器下载完整包和准确同名的 checksum，经 SHA256、包路径、版本、target、tag 源码提交、协议及 data epoch 检查后，建立：

```text
~/.local/share/vibeface/releases/<version>/
~/.local/share/vibeface/current -> releases/<version>
~/.local/bin/vibeface -> ~/.local/share/vibeface/current/vibeface
```

它不创建节点数据、不创建个人网络、不启动后台服务、不安装 Provider，也不开启自动更新。安装后按提示把 `~/.local/bin` 加入 PATH，或直接使用该目录下的 CLI。SHA256 与包同由 GitHub 提供，不能替代独立发布签名。

已有 `current` 或同名 bin 文件时，安装器保留原文件并拒绝安装。包协议 2 的受管安装通过 `update apply` 升级，不能绕过正在运行节点的维护检查。包协议 1 的旧预览版需要 [一次性手动更换程序](releasing.md#手动建立受管安装或更换程序)，不支持自动跨命名更新。不要只复制 binary；更新和服务管理需要完整包。

自定义位置需使用绝对路径，并保持安装根目录和 bin 目录为普通目录：

```sh
bash install.sh --channel nightly \
  --install-root /absolute/path/to/vibeface \
  --bin-dir /absolute/path/to/bin
```

## 初始化与前台运行

```sh
vibeface --version
vibeface --help
vibeface
```

直接运行默认使用 `~/.vibeface`。首次仅在不存在或为空的真实目录中创建本机身份与配置，监听 `127.0.0.1:4790`，不开放局域网，不允许执行目录，不启用管理 Agent 或发现服务。当前格式的已有配置按原身份启动。旧格式、损坏、非空未初始化或符号链接目录不会被重置。旧格式在任何数据库写入或 Provider 启动前拒绝。启动不自动输出配对秘密。保持前台进程运行，在另一个终端执行：

```sh
vibeface access
```

在可信浏览器打开本机链接，创建个人网络并保存加密恢复包。已有个人网络时应配对，不能重新创建网络代替配对。见 [添加自己的设备](node.md#添加自己的设备)。编码执行需要另装并登录 Codex 等 Provider；聊天调度需要 [配置管理 Agent](node.md#配置管理-agent)。

使用自定义数据目录可以直接执行 `vibeface --data-dir /absolute/path/to/data`。根 `--data-dir` 只用于直接启动；有子命令时，把该参数写在子命令后，例如 `vibeface status --data-dir /absolute/path/to/data`。混用根参数与子命令会报错，避免操作错误的数据目录。

要在首次启动前设置执行目录，使用显式初始化：

```sh
vibeface init --root /absolute/path/to/projects
vibeface
```

`init` 拒绝覆盖非空数据目录。显式 `vibeface serve` 只启动已初始化节点。当前格式的已有节点先停止进程，再修改原 `config.json`，不要重新初始化身份。epoch 2 及更早的数据目录不兼容；保留旧目录，使用另一个空目录初始化新节点。

## 用 CLI 管理后台服务

先停止前台进程，再安装当前用户的后台服务：

```sh
vibeface service install
vibeface service status
vibeface status
vibeface service stop
vibeface service start
vibeface service restart
```

macOS 使用 launchd 用户服务，Linux 使用 `systemd --user`。CLI 管理当前用户的一套受管节点服务，服务通过固定的 `current` 路径启动，以便升级后使用新程序。卸载服务保留数据：

```sh
vibeface service uninstall
```

使用自定义数据目录时，在对应节点命令中显式传入 `--data-dir /absolute/path/to/data`。服务命令不能用另一个数据目录操作已绑定的节点服务。更多平台要求见 [作为后台应用运行](node.md#作为后台应用运行)。

## 选择更新渠道

首次下载渠道与运行时更新策略分别配置。默认运行时策略是 Stable，自动更新关闭。Nightly 安装初始化后建议明确配置：

```sh
vibeface update configure --channel nightly --automatic false
vibeface update status
vibeface update check
vibeface update apply
```

手动应用更新需已安装并运行用户服务，更新器才可取得维护租约并确认重启健康状态。希望在空闲时自动更新：

```sh
vibeface update configure --channel nightly --automatic true
```

将来改用正式版：

```sh
vibeface update configure --channel stable --automatic true
```

Stable 不存在时不会切换 Nightly，更新器也不自动降级。自定义安装根需在首次 `update configure` 时加上 `--install-root /absolute/path/to/vibeface`。更新行为、维护检查和备份边界见 [构建与发布](releasing.md#更新渠道)。
