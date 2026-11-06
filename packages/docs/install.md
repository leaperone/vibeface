---
title: 安装与管理节点
description: 在自己的 Mac 或 Linux 设备上安装 Vibeface Nightly，并初始化本机 Node。
---

# 安装与管理节点

第一次使用可以先按[快速上手](quick-start.md)完成安装、建网和第一次对话。本文提供安装方式、后台服务和更新命令的详细说明。

## 先核对安装包版本

本文对应拆分后的 CLI、Core 与 App。完整包需要同时含 `vibeface`、`vibeface-core` 和 `web/`，当前 main 的安装器会检查这个结构。

安装器只接受当前协议和 data epoch 的完整包。旧开发版的数据目录、组件包和单独程序不能沿用；请创建全新数据目录。需要从源码运行时，按[从源码准备当前版本](quick-start.md#从源码准备当前版本)操作。下面的安装和更新步骤以匹配的完整包已经发布为前提。

## 选择安装目录

本文的命令默认使用数据目录 `~/.vibeface`，可用 `--data-dir` 或 `VIBEFACE_DATA_DIR` 指定其他绝对路径，见[部署配置](deployment.md)。首次安装前设置安装目录和命令目录，例如：

```sh
export VIBEFACE_INSTALL_ROOT="$HOME/.local/share/vibeface"
export VIBEFACE_BIN_DIR="$HOME/.local/bin"
```

已有安装使用原来的路径，不要另建目录绕过更新。安装后将 `VIBEFACE_BIN_DIR` 加入 PATH，或用完整路径运行 CLI。

Vibeface 的完整 Rust 包包含 CLI `vibeface`、后台 Core `vibeface-core` 和 `web/` 静态 App。支持 macOS Apple Silicon、Windows x64、Linux x64 和 Linux ARM64；Linux 包需要 glibc。macOS 发布包的两个程序使用 Developer ID 签名并通过 Apple notarization。系统设置保持默认的“自动允许下载的已签名软件接收传入连接”且未阻止所有传入连接时，应用防火墙直接放行，安装和更新后不再询问。

## npm Nightly

Node.js 22+ 可通过已公开的 npm 包安装 Nightly：

```sh
npm install -g @leaperone/vibeface@nightly
vibeface --version
vibeface
```

一个 npm 包包含四个平台的 CLI、Core 和 `web/` 资源，按本机平台启动 Rust CLI。无需安装期脚本、二次下载、Rust 编译器或构建工具。npm 不创建身份、配置 Provider、启动服务或开启自动更新。产品 Nightly 的 GitHub Release 发布成功后自动通过 OIDC 发布同一份 npm 包，流程与权限要求见 [npm Nightly 制品与发布](releasing.md#npm-nightly-制品与发布)。也可从扩展 CI 的 `npm-package-<SHA>` artifact 下载 `.tgz`，使用 `npm install --global --prefix /absolute/isolated/prefix --ignore-scripts /absolute/path/leaperone-vibeface-<version>.tgz` 隔离安装。

初始化后，下面的 `service` 命令也适用于全局 npm 安装。服务绑定 npm 包中固定的原生程序路径；不要把后台服务绑定到临时 `npx` 缓存，也不要在服务仍运行时移除或覆盖 npm 安装。迁移服务前先从原安装入口确认空闲并停止服务；npm 服务安装拒绝运行中的 updater、仍被运行中节点锁定或存在未完成更新记录的数据目录，通过后才移除同一目录绑定的旧受管更新定时器。安装期间持续持有同一更新锁，避免旧 updater 并发改写状态。存在 `update-pending.json` 时，先使用原受管安装的 `update apply` 恢复，再迁移服务。

npm 安装由 npm 更新，内建 `update apply`、`update check`、`update configure` 会提示使用 npm，`update status` 报告 npm 来源及关闭的自动更新。先等待操作结束并对账所有结果未知的写入，再停止后台服务、更新和重启：

```sh
vibeface service stop
npm install -g @leaperone/vibeface@nightly
```

再启动服务并核对状态：

```sh
vibeface service start
vibeface status
```

前台节点先正常退出再更新。更新不转换数据库，不跨数据 epoch 复用旧目录。卸载前先执行 `vibeface service uninstall`，再运行 `npm uninstall -g @leaperone/vibeface`；数据目录保留。

## GitHub 完整包安装

首次安装最新正式版，默认渠道是 Stable：

```sh
curl -fsSL https://raw.githubusercontent.com/leaperone/vibeface/main/install.sh | bash -s -- --install-root "$VIBEFACE_INSTALL_ROOT" --bin-dir "$VIBEFACE_BIN_DIR"
```

当前尚无 Stable Release，使用上述命令会明确失败。要安装开发预览版，请显式选择 Nightly：

```sh
curl -fsSL https://raw.githubusercontent.com/leaperone/vibeface/main/install.sh | bash -s -- --channel nightly --install-root "$VIBEFACE_INSTALL_ROOT" --bin-dir "$VIBEFACE_BIN_DIR"
```

安装器依赖系统 Bash、curl、tar、gzip 和 SHA256 工具，不需要 Python、jq、Rust 或 Node.js。它按数值版本选择已公开的渠道版本，不选择 rc，也不会因最新包缺少当前平台而退回旧版。网络或 GitHub API 限流失败会保留错误。

如需先检查脚本，再运行：

```sh
curl -fsSL https://raw.githubusercontent.com/leaperone/vibeface/main/install.sh -o install.sh
bash install.sh --help
bash install.sh --channel nightly --install-root "$VIBEFACE_INSTALL_ROOT" --bin-dir "$VIBEFACE_BIN_DIR"
```

安装器下载完整包和准确同名的 checksum，经 SHA256、包路径、版本、target、tag 源码提交、协议及 data epoch 检查后，建立：

```text
<安装根目录>/releases/<version>/
<安装根目录>/current -> releases/<version>
<命令目录>/vibeface -> <安装根目录>/current/vibeface
```

它不创建节点数据、不创建个人网络、不启动后台服务、不安装 Provider，也不开启自动更新。安装后把明确指定的 `--bin-dir` 加入 PATH，或直接使用该目录下的 CLI。SHA256 与包同由 GitHub 提供，不能替代独立发布签名。

已有 `current` 或同名 bin 文件时，安装器保留原文件并拒绝安装。 旧版 CLI 只识别 `cli-v`，首次切换统一产品 Tag 需要手动安装新版完整包。先确认旧任务与更新已结束，关闭旧自动更新并停止旧服务，再安装到新的安装目录与命令目录，由新入口注册服务。保留原数据目录；这次 Tag 调整不改变数据 epoch。新版本之后的更新器只跟踪 `v`，不会回退旧应用前缀。受管安装通过 `update apply` 升级，不能绕过正在运行节点的维护检查。更新和服务管理需要完整包。

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

数据目录默认为 `~/.vibeface`，可用 `--data-dir` 或 `VIBEFACE_DATA_DIR` 指定，见[部署配置](deployment.md)。首次仅在空目录或不存在的目录中创建本机身份与配置，监听 `127.0.0.1:4790`，不开放局域网，不允许执行目录，不启用管理 Agent，不配置 relay 或公共入口。已有配置按原身份启动；损坏、非空未初始化或符号链接目录不会被重置。启动不自动输出配对秘密。

保持进程运行，在本机可信浏览器打开 `http://localhost:4790`。首屏选**新建我的网络**，填写这个节点的名称及恢复包口令（浏览器名称自动生成），下载并确认保存加密恢复包后创建个人网络，不需要复制终端邀请。已有网络的节点会尝试核验当前浏览器；新浏览器可使用本机自动访问，永久成员授权仍需原有成员邀请或恢复包。使用恢复包时，本机入口和初始化授权自动选取。

另一个节点加入已有个人网络时，不要在它的本机网页上建网。在管理设备的**添加设备**里选**节点**，生成加入链接，然后任选一种方式：在新节点的本机网页上选**加入已有网络**并粘贴链接；或者在新节点的终端执行 `vibeface join '<加入链接>' --name '<设备显示名称>'`。两种方式都不用停止 Node：Node 正在运行时，命令交给它加入。数据目录为空或不存在时，`join` 先按直接启动的默认配置初始化；要预设执行目录或 relay，可先运行 `vibeface init`。网页或终端显示 6 位安全码，在管理设备上核对一致后点**一致，允许**。中断后用同一链接和目录重试，沿用原请求。见 [添加自己的设备](node.md#添加自己的设备)。`vibeface access` 保留为本机网页的备用入口。

使用签名管理身份或可信本机会话进入工作区后，在**设置**中选择管理 Agent、模型和允许 Agent 访问的项目目录。默认 Agent 工作目录在首次保存时自动创建，无需编辑配置文件或重启 Node。编码执行和管理 Agent 仍需事先安装并登录对应 Provider，角色授权也必须满足要求。见 [配置管理 Agent](node.md#配置管理-agent)。

使用自定义数据目录可以直接执行 `vibeface --data-dir /absolute/path/to/data`。根 `--data-dir` 只用于直接启动；有子命令时，把该参数写在子命令后，例如 `vibeface status --data-dir /absolute/path/to/data`。混用根参数与子命令会报错，避免操作错误的数据目录。

要在首次启动前设置执行目录，使用显式初始化：

```sh
vibeface init --root /absolute/path/to/projects
vibeface
```

`init` 拒绝覆盖非空数据目录。显式 `vibeface serve` 只启动已初始化节点。已有节点先停止进程，再修改原 `config.json`，不要重新初始化身份。

Node 之间经 iroh 连接。不能直连的 Node 经个人网络中另一台 Node 运行的 relay 转发，在初始化时指定：

```sh
vibeface init --relay-listen 127.0.0.1:3340 --relay-url https://relay.example   # 运行 relay 的公网 Node
vibeface init --relay https://relay.example                                      # 其他 Node，可重复
```

设置写入 `config.json` 的 `network` 字段，之后修改该字段并重启。程序不内置公共 relay。更多说明见 [Node 之间的连接](node.md#node-之间的连接)。

要让手机从外网打开网页而公网机器不保存你的数据，在公网机器上运行公共入口，个人设备上的 Node 指向它：

```sh
vibeface init --public-entry --public-url https://vb.example.com --relay-listen 127.0.0.1:3340 --relay-url https://vb.example.com   # 公网机器
vibeface init --entry https://vb.example.com --relay-token-file /path/to/entry-token             # 个人设备上的 Node
```

反向代理和令牌的处理见 [配置公共入口](node.md#配置公共入口)。

前台运行时日志同时写到数据目录的 `node.log`，用 `vibeface logs` 查看。`vibeface status` 显示写入状态和每台对端最近一次同步，出问题时先看 [排障](troubleshooting.md)。

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

macOS 使用 launchd 用户服务，Linux 使用 `systemd --user`，Windows 使用当前用户的计划任务。CLI 管理当前用户的一套节点服务。GitHub 受管安装通过固定的 `current` 路径启动；npm 安装通过 npm 包中固定的原生程序路径启动。卸载服务保留数据：

```sh
vibeface service uninstall
```

使用自定义数据目录时，在对应节点命令中显式传入 `--data-dir /absolute/path/to/data`。服务命令不能用另一个数据目录操作已绑定的节点服务。更多平台要求见 [作为后台应用运行](node.md#作为后台应用运行)。

## 选择更新渠道

本节只适用于 GitHub 受管安装。npm 安装按上面的 npm 命令更新。

首次下载渠道与运行时更新策略分别配置。更新器没有默认策略，必须明确指定渠道、是否自动更新及安装根目录。Nightly 安装初始化后配置：

```sh
vibeface update configure --channel nightly --automatic false --install-root "$VIBEFACE_INSTALL_ROOT"
vibeface update status
vibeface update check
vibeface update apply
```

手动应用更新需已安装并运行用户服务，更新器才可取得维护租约并确认重启健康状态。希望在空闲时自动更新：

```sh
vibeface update configure --channel nightly --automatic true --install-root "$VIBEFACE_INSTALL_ROOT"
```

将来改用正式版：

```sh
vibeface update configure --channel stable --automatic true --install-root "$VIBEFACE_INSTALL_ROOT"
```

Stable 不存在时不会切换 Nightly，更新器也不自动降级。每次 `update configure` 都必须传入实际安装根目录的绝对路径。更新行为、维护检查和备份边界见 [构建与发布](releasing.md#更新渠道)。


## 安装桌面应用

从同一个产品 Release 下载适合本机的 `vibeface-desktop-<version>-<target>`。macOS Apple Silicon 使用 `.dmg`，Windows x64 使用 `.exe` 安装器。两端也提供 `.zip`，以及校验摘要和来源收据。

桌面应用内含同版本、同源码的 CLI、Core 和 Web 资源。关闭窗口后继续在托盘运行。选择退出时，应用停止自己启动的 Core；连接既有服务时，退出保留该服务。设置里的运行目录决定这台电脑的数据位置。

Windows 安装器只安装到当前账户，不需要管理员、WSL、Git Bash 或开发者模式。未配置签名证书的 Nightly 明确标记为未签名开发预览。正式版必须通过签名验证。已有独立 CLI 服务可以继续运行；桌面应用只连接它，不接管启停。

## 安装 Windows CLI 和 Core

Windows 完整包使用 `x86_64-pc-windows-msvc` 目标，包含 `vibeface.exe`、`vibeface-core.exe`、`web/` 和 `scripts/install-node-service.ps1`。保持整个目录完整。Windows 数据目录使用当前账户的 SID 权限，本机维护接口使用仅当前账户可访问的命名管道。

源码中的 `scripts/install.ps1 -Channel nightly` 安装最新包含 Windows 制品的产品 Nightly，使用目录 junction 和 `.cmd` 启动入口。该脚本不创建节点、安装后台服务或开启自动更新。初始化后，按 `vibeface service --help` 配置当前账户的后台任务。

CLI 更新器只管理独立安装。更新桌面应用时安装新的桌面包，不能使用 CLI 更新器改写签名应用内的程序。
