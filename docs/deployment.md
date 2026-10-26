# 配置并部署 Node

先为每台 Node 选择数据目录、程序安装目录和需要启用的集成。下文的 `<...>` 都是占位符，运行前替换成部署环境中的实际值。配置保存在本机，不提交到仓库。

## 指定数据目录

```sh
export VIBEFACE_DATA_DIR='<数据目录的绝对路径>'
vibeface init --data-dir "$VIBEFACE_DATA_DIR"
vibeface serve --data-dir "$VIBEFACE_DATA_DIR"
```

所有读取或写入 Node 数据的命令都要求 `--data-dir` 或 `VIBEFACE_DATA_DIR`。直接运行 `vibeface` 也要求这个配置，并只在明确指定的空目录初始化。程序不使用当前目录或用户 home 作为兜底。

默认只监听回环地址，不开放项目目录，不启用执行 Provider，也不扫描其他应用的会话。要允许访问项目，在初始化时重复传入 `--root '<项目的绝对路径>'`，或在有管理权限的设备设置中配置。

## 选择设备名称

在建网页面填写设备名称。加入已有网络时显式指定名称：

```sh
vibeface join '<加入链接>' --name '<设备显示名称>' --data-dir "$VIBEFACE_DATA_DIR"
```

程序不读取系统主机名。未授权连接只能取得通用 Node 信息；实际名称、网络 ID 和入口信息要求有效成员身份或本机会话。

## 配置 SDK 与 App Server

先安装所选 Provider，再配置程序路径、工作目录和认证环境。修改已有 `config.json` 时保留未列出的字段。

用到 Codex 或 Cursor 而路径缺失时，Node 在 `init`、启动和网页打开开关时从本机找出并写进配置，之后照写下的用，不再每次猜：

- Codex 目录：Node 进程的 `CODEX_HOME`，没有时用 `$HOME/.codex`，目录必须存在。
- App Server：`<Codex 目录>/app-server-control/app-server-control.sock` 存在时用它（`system`）；不存在时改为 `spawn`，并把 Codex 目录写进 `provider-env.json` 的 `CODEX_HOME`。
- 会话目录：`<Codex 目录>/sessions`。
- 只写了名字的程序（`codex`、`cursor-sdk-bridge`）在 PATH 里找不到时，再找 `~/.local/bin`、`/opt/homebrew/bin`、`/usr/local/bin`，Codex 还找 `/Applications/Codex.app/Contents/Resources`。服务进程的 PATH 很短，这一步让它们也能找到。

已写下的路径不被替换。找不到 Codex 目录时打开开关会报错，在网页里手动填写，或按下面写进配置。密钥不在查找范围内。

使用已经运行的 Codex App Server 时，也可以直接给出它的 socket：

```json
{
	"codex_bin": "<Codex 程序的绝对路径>",
	"providers": {
		"execution": true,
		"codex_app_server": {
			"mode": "system",
			"socket": "<App Server socket 的绝对路径>"
		}
	}
}
```

Node 只连接这个 socket，不搜索其他正在运行的会话，也不启动或停止其所属进程。

由 Node 启动自己的 App Server 时，将模式改成 `spawn`，并在私有的 `provider-env.json` 中指定该实例使用的目录：

```json
{
	"codex": {
		"CODEX_HOME": "<该 Codex 实例的数据目录绝对路径>"
	},
	"cursor": {}
}
```

Provider 的其他认证环境通过设备设置或服务配置填写。不要把密钥写入源码、URL、共享记录或命令历史。Cursor 的 bridge 路径和协调 Agent 工作目录也由本机配置指定。

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

安装 hooks 时也必须明确选择被修改的配置所在 home：

```sh
vibeface hooks install --data-dir "$VIBEFACE_DATA_DIR" --home '<包含 .codex 和 .claude 的目录>'
vibeface hooks status --data-dir "$VIBEFACE_DATA_DIR" --home '<同一个目录>'
```

本地执行保留所需的真实路径。共享事件和 webhook 使用工作区相对标识，日志隐藏已配置的私有目录。诊断导出提供配置摘要，不输出本机配置文件全文。

## 配置网络入口

默认回环监听适用于本机浏览器。需要其他设备访问时，明确配置监听地址和外部访问 URL。非回环监听必须提供 `--public-url`。运行 relay 时必须同时提供 `--relay-listen` 和 `--relay-url`。

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
vibeface update configure --channel nightly --automatic false --install-root "$VIBEFACE_INSTALL_ROOT" --data-dir "$VIBEFACE_DATA_DIR"
vibeface service install --data-dir "$VIBEFACE_DATA_DIR"
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
