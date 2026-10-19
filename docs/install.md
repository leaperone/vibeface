# 安装与管理节点

Vibeface 的主程序是 Rust 二进制 CLI `vibeface`，网页随程序内嵌分发。支持 macOS Apple Silicon、Linux x64 和 Linux ARM64；Linux 包需要 glibc。macOS 包尚无 Apple 开发者签名或 notarization。

## npm Nightly

Node.js 22+ 可通过 npm 安装 Nightly。npm 包尚未公开发布，首次发布后使用：

```sh
npm install -g @leaperone/vibeface@nightly
vibeface --version
vibeface
```

一个 npm 包包含三个平台的完整程序及随包资源，按本机平台启动 Rust 程序。无需安装期脚本、二次下载、Rust 编译器或构建工具。npm 不创建身份、配置 Provider、启动服务或开启自动更新。发布前从 PR 的 `npm-package-<SHA>` CI artifact 下载 `.tgz`，可用 `npm install --global --prefix /absolute/isolated/prefix --ignore-scripts /absolute/path/leaperone-vibeface-<version>.tgz` 隔离安装。

初始化后，下面的 `service` 命令也适用于全局 npm 安装。服务绑定 npm 包中固定的原生程序路径；不要把后台服务绑定到临时 `npx` 缓存，也不要在服务仍运行时移除或覆盖 npm 安装。迁移服务前先从原安装入口确认空闲并停止服务；npm 服务安装拒绝运行中的 updater、仍被运行中节点锁定或存在未完成更新记录的数据目录，通过后才移除同一目录绑定的旧受管更新定时器。安装期间持续持有同一更新锁，避免旧 updater 并发改写状态。存在 `update-pending.json` 时，先使用原受管安装的 `update apply` 恢复，再迁移服务。

npm 安装由 npm 更新，内建 `update apply`、`update check`、`update configure` 会提示使用 npm，`update status` 报告 npm 来源及关闭的自动更新。先等待操作结束并对账所有结果未知的写入，再停止后台服务、更新和重启：

```sh
vibeface service stop
npm install -g @leaperone/vibeface@nightly
```

若使用随包 Codex SDK 管理 Agent，首次安装及每次升级后都需为新包安装锁定依赖。将下面的 `<target>` 替换为本机平台：macOS ARM64 使用 `aarch64-apple-darwin`，Linux x64 使用 `x86_64-unknown-linux-gnu`，Linux ARM64 使用 `aarch64-unknown-linux-gnu`。隔离安装时，改用对应 prefix 下的包路径。

```sh
npm ci --prefix "$(npm root -g)/@leaperone/vibeface/vendor/<target>/sidecars" --omit=dev --ignore-scripts
```

完成所需依赖安装后，再启动服务并核对状态：

```sh
vibeface service start
vibeface status
```

前台节点先正常退出再更新。更新不转换数据库，不跨数据 epoch 复用旧目录。卸载前先执行 `vibeface service uninstall`，再运行 `npm uninstall -g @leaperone/vibeface`；数据目录保留。

## GitHub 完整包安装

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

它不创建节点数据、不创建集群、不启动后台服务、不安装 Provider，也不开启自动更新。安装后按提示把 `~/.local/bin` 加入 PATH，或直接使用该目录下的 CLI。SHA256 与包同由 GitHub 提供，不能替代独立发布签名。

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

直接运行默认使用 `~/.vibeface`。首次仅在不存在或为空的真实目录中创建本机身份与配置，监听 `127.0.0.1:4790`，不开放局域网，不允许执行目录，不启用管理 Agent，默认不配置发现入口。已有配置按原身份启动；损坏、非空未初始化或符号链接目录不会被重置。启动不自动输出配对秘密。

保持进程运行，在本机可信浏览器打开 `http://localhost:4790`。填写浏览器名称及恢复包口令，下载并确认保存加密恢复包后创建协作集群，不需要复制终端邀请。已有集群的节点会尝试核验当前浏览器；新浏览器可使用本机自动访问，永久成员授权仍需原有成员邀请或恢复包。使用恢复包时，本机入口和初始化授权自动选取。

另一台节点加入已有协作集群时，在它的 localhost 页面选择**使用节点邀请加入**，粘贴原管理客户端生成的 Node 邀请。双方分别核对节点身份和权限后确认。刷新页面只核对原申请，不重新提交。不能新建集群代替配对。见 [添加自己的设备](node.md#添加自己的设备)。`vibeface access` 和 `vibeface pair` 保留为备用方式。

使用签名管理身份或可信本机会话进入工作区后，在**设置**中选择管理 Agent、模型和允许 Agent 访问的项目目录。默认 Agent 工作目录在首次保存时自动创建，无需编辑配置文件或重启 Node。编码执行和管理 Agent 仍需事先安装并登录对应 Provider，角色授权也必须满足要求。见 [配置管理 Agent](node.md#配置管理-agent)。

使用自定义数据目录可以直接执行 `vibeface --data-dir /absolute/path/to/data`。根 `--data-dir` 只用于直接启动；有子命令时，把该参数写在子命令后，例如 `vibeface status --data-dir /absolute/path/to/data`。混用根参数与子命令会报错，避免操作错误的数据目录。

要在首次启动前设置执行目录，使用显式初始化：

```sh
vibeface init --root /absolute/path/to/projects
vibeface
```

`init` 拒绝覆盖非空数据目录。显式 `vibeface serve` 只启动已初始化节点。已有节点先停止进程，再修改原 `config.json`，不要重新初始化身份。

要使用自己的发现或中继入口，在初始化或启动时传入 URL：

```sh
vibeface --bootstrap-url https://relay.example
vibeface serve --bootstrap-url wss://relay.example/api/network/ws
vibeface init --data-dir /absolute/path/to/new-node --bootstrap-url https://relay.example
```

这三种命令分别使用，可重复 `--bootstrap-url` 指定多个入口，最多 8 个。HTTP(S) 地址自动转换成 WS(S) 的 `/api/network/ws` 端点，URL 不接受凭证、查询参数或片段。HTTP(S) 仅接受根路径或 `/api/network/ws`，WS(S) 保留完整的根路径和自定义路径。程序不内置公共入口。首次直接启动和显式 `init` 将传入的 URL 保存到配置；已有节点启动时，传入的列表仅覆盖本次运行的 `network.bootstrap_urls`，不改写配置或身份。省略该参数时使用原配置。更多说明见[发现与公共中继](node.md#发现与公共中继)。

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

macOS 使用 launchd 用户服务，Linux 使用 `systemd --user`。CLI 管理当前用户的一套节点服务。GitHub 受管安装通过固定的 `current` 路径启动；npm 安装通过 npm 包中固定的原生程序路径启动。卸载服务保留数据：

```sh
vibeface service uninstall
```

使用自定义数据目录时，在对应节点命令中显式传入 `--data-dir /absolute/path/to/data`。服务命令不能用另一个数据目录操作已绑定的节点服务。更多平台要求见 [作为后台应用运行](node.md#作为后台应用运行)。

## 选择更新渠道

本节只适用于 GitHub 受管安装。npm 安装按上面的 npm 命令更新。

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
