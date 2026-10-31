---
title: 配置并部署 Node
description: 配置数据目录、网络边界与公共入口。
---

# 配置并部署 Node

先为每台 Node 选择数据目录、程序安装目录和需要启用的集成。下文的 `<...>` 都是占位符，运行前替换成部署环境中的实际值。配置保存在本机，不提交到仓库。部署前先了解 [Agent 分工](bot-and-execution.md) 与 [隐私和执行权限](privacy-security.md)。

## 指定数据目录

默认数据目录是 `~/.vibeface`。要使用其他目录，在命令中传入 `--data-dir '<数据目录的绝对路径>'`，或设置环境变量：

```sh
export VIBEFACE_DATA_DIR='<数据目录的绝对路径>'
vibeface init
vibeface serve
```

读取或写入 Node 数据的命令依次使用 `--data-dir`、`VIBEFACE_DATA_DIR` 和 `~/.vibeface`，路径必须是绝对路径。取不到 `HOME` 或 `HOME` 是相对路径时报错，需显式指定目录。直接运行 `vibeface` 只在所选目录为空或不存在时初始化。程序不使用当前目录。

网页 HTTP 入口默认监听回环地址，Node 间 iroh UDP 传输默认绑定全接口。默认不开放项目目录，不启用执行 Provider，也不扫描其他应用的会话。新任务可用目录通过初始化参数 `--root '<项目的绝对路径>'` 或有管理权限的设备设置配置；这不是操作系统沙箱。

## 选择设备名称

在建网页面填写设备名称。加入已有网络时显式指定名称：

```sh
vibeface join '<加入链接>' --name '<设备显示名称>'
```

程序不读取系统主机名。未授权连接只能取得通用 Node 信息；实际名称、网络 ID 和入口信息要求有效成员身份或本机会话。

## 配置 SDK 与 App Server

先安装所选 Provider，再配置程序路径、工作目录和认证环境。修改已有 `config.json` 时保留未列出的字段。

用到 Codex 或 Cursor 而程序路径缺失时，Node 在 `init`、启动和网页打开开关时从本机找出并写进配置，之后照写下的用，不再每次猜。Codex 的运行数据由 Node 隔离管理：

- 默认 App Server 模式是 `spawn`，Node 使用 `<data-dir>/providers/codex` 作为 `CODEX_HOME`，并在 `<data-dir>/providers/codex/sessions` 保存自己的会话。
- Node 不扫描 `$HOME/.codex` 或用户 control socket。只有明确选择 `system` 并填写绝对 socket 才连接外部 App Server。
- 只写了名字的程序（`codex`、`cursor-sdk-bridge`）在 PATH 里找不到时，再找 `~/.local/bin`、`/opt/homebrew/bin`、`/usr/local/bin`，Codex 还找 `/Applications/Codex.app/Contents/Resources`。服务进程的 PATH 很短，这一步让它们也能找到。

已写下的程序路径不被替换。Node-owned Codex 运行目录由 Node 创建和维护，不从用户目录查找，也不在普通 Provider 环境变量中修改。密钥不在查找范围内。

使用已经运行的 Codex App Server 时，也可以直接给出它的 socket：

```json
{
	"bins": {
		"codex": "<Codex 程序的绝对路径>"
	},
	"providers": {
		"execution": ["codex"],
		"codex_app_server": {
			"mode": "system",
			"socket": "<App Server socket 的绝对路径>"
		}
	}
}
```

Node 只连接这个 socket，不搜索其他正在运行的会话，也不启动或停止其所属进程。

由 Node 启动自己的 App Server 时只需保持 `spawn`。Node 会在启动边界注入 Node-owned `CODEX_HOME`，覆盖普通 Provider 环境里的同名值。Provider 的其他认证环境通过设备设置或服务配置填写。不要把密钥写入源码、URL、共享记录或命令历史。Cursor 的 bridge 路径和协调 Agent 工作目录也由本机配置指定。

## 配置会话扫描和 hooks

会话扫描默认关闭。需要扫描时，在 `config.json` 的 `triage` 中配置：

```json
{
	"default_outlet": "record",
	"projects": [],
	"webhook_url": null,
	"scan_codex_sessions": true,
	"codex_sessions": "<Codex sessions 目录的绝对路径>"
}
```

安装 hooks 默认改 Node-owned 的 Claude/Codex 根，不读取用户交互会话。改用户目录时显式使用 `--home`；该模式只改变 hook 文件位置，不让 Bot 接入用户会话。写入的文件见 [回答本人自己开的会话](node.md#回答本人自己开的会话)：

```sh
vibeface hooks install [--home '<包含各 Agent 配置的目录>']
vibeface hooks status [--home '<同一个目录>']
```

本地执行保留所需的真实路径。共享事件和 webhook 使用工作区相对标识，日志隐藏已配置的私有目录。诊断导出提供配置摘要，不输出本机配置文件全文。

## 配置网络入口

网页 HTTP 入口的默认回环监听适用于本机浏览器。需要其他设备访问网页入口时，明确配置监听地址和外部访问 URL。非回环监听必须提供 `--public-url`。运行 relay 时必须同时提供 `--relay-listen` 和 `--relay-url`。Node 间的 iroh UDP 传输默认绑定全接口，不能把网页回环监听当作它的网络边界。

公网部署使用 HTTPS 反向代理，将网页和 WebSocket 请求转发到配置的回环监听地址。域名、证书、防火墙和 relay 地址都属于部署配置。具体角色配置见 [Node 使用](node.md#配置公共入口)。

## 安装程序和后台服务

选择安装目录后执行安装脚本：

```sh
export VIBEFACE_INSTALL_ROOT='<程序安装目录的绝对路径>'
export VIBEFACE_BIN_DIR='<命令目录的绝对路径>'
bash install.sh --channel nightly --install-root "$VIBEFACE_INSTALL_ROOT" --bin-dir "$VIBEFACE_BIN_DIR"
```

使用受管更新或原生后台服务前，登记安装目录：

```sh
vibeface update configure --channel nightly --automatic false --install-root "$VIBEFACE_INSTALL_ROOT"
vibeface service install
```

服务使用部署时选定的程序和数据目录。不要依赖交互式 shell 的 PATH；后台 Provider 建议使用绝对程序路径。macOS 使用当前用户的 LaunchAgent，Linux 使用当前用户的 systemd 服务，服务管理器的标准目录由平台决定。

## 从源码生成分发包

```sh
bash scripts/build-web-crypto.sh
bash scripts/build-web.sh
bash scripts/package-node.sh
```

前两个步骤清理各自的输出目录，再记录相关源码和输出文件摘要。后续步骤校验来源、文件内容和完整文件清单，拒绝过期或多余资源。原生程序每次重新构建，不提供跳过构建的打包入口。只打包受版本控制的文档，忽略目录中的私有笔记不会进入发布包。

完整工具版本和平台验收要求见 [构建与发布](releasing.md)。
