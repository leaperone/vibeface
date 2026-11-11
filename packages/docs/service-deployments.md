---
title: 发布官方服务部署
description: 使用已发布制品，在本仓库部署官方容器服务。
---

# 发布官方服务部署

制品发布和服务部署是两个独立操作。`release product` 和 `release www` 只发布不可变制品，不改变线上服务。部署命令显式选择已经完成发布的精确 Tag，不使用分支、`latest` 或未完成 Release。

所有部署命令、Dockerfile、Compose 与 GitHub Actions 都在本仓库维护。部署不依赖另一个发布仓库，不重新编译产品，不发布新的产品版本。

## 执行独立部署

先只读核对，再执行部署发布：

```sh
python3 scripts/deploy.py plan --service all --tag '<PRODUCT_TAG>'
python3 scripts/deploy.py release --service all --tag '<PRODUCT_TAG>'
```

`--service` 支持以下目标：

| 目标 | GitHub Actions 入口 | 交付内容 |
| --- | --- | --- |
| `all` | Deploy / Product services（`deploy-product.yml`） | 在同一次运行中串行部署官方公共节点和 App，不含官网 |
| `node` | Deploy / Node（`deploy-node.yml`） | 香港官方公共节点的 Core 容器：全网 bootstrap 节点、开放 relay 和浏览器接入，各有限额，不含前端文件 |
| `app` | Deploy / Web App（`deploy-app.yml`） | 独立网页静态容器，只提供静态文件；网页直接连公共节点的 `gateway_url` |
| `www` | Deploy / Website（`deploy-www.yml`） | 独立官网静态容器，使用精确 `www-v<version>` Tag |

产品目标接受精确 Stable、RC、Nightly `v<version>`，官网只接受对应 `www-v<version>`。不自动跟随渠道。每个入口固定部署目标，只接收 Tag 和 `apply`；`deploy-services.yml` 是共用的可复用实现，不接受手动派发。所有入口共用该实现中的全局部署并发锁，正在运行的部署不会被后续请求取消。部署命令从 main 以 `apply=true` 启动对应入口；UI 手动运行默认 `apply=false`，只验证制品、构建容器并冒烟，不接触服务器。运行名称包含目标、Tag、执行意图和分支，便于核对运行记录。

```sh
python3 scripts/deploy.py release --service www --tag '<WEBSITE_TAG>'
gh workflow run deploy-node.yml --ref '<BRANCH>' -f tag='<PRODUCT_TAG>' -F apply=false
```

真实激活仅允许从 main 执行。派发成功仅表示 GitHub 接收请求，须继续跟踪对应入口到终态；派发响应未知时先查询相同 Tag 的运行记录，不能直接重发。每次运行重新核对最终 `release.json` 的 Tag、源码 SHA、共享发布来源规则（main 或受信机械 Nightly）、附件集合和摘要，再下载需要的组件并核对实际字节与包内来源。容器只包装这些已发布字节，通过 SSH 传输并按 image ID 激活，无需共享镜像仓库。

激活前同时核对传输归档的 SHA256 和导入后的 image ID。归档中的镜像必须与调用方指定的 `--image` 一致；摘要正确不能替代镜像身份校验。

`node` 无论是否 `apply` 都先在 runner 演练。脚本选择候选发布时间之前最近的完整产品 Release，排除候选本身、Draft 和缺少最终 manifest 或必要组件的发布，并核对其 `release.json`、来源和附件摘要。旧 Core 初始化并启动隔离 fixture 后，脚本原子换入候选 Core 生成的完整配置，再激活候选镜像并验证重复激活、原身份和初始化数据保留。fixture 对外公布的地址（`public_url`、`relay_url`、bootstrap anchor）都是回环，Core 仍按公共节点配置以 host network 监听 runner 的 `0.0.0.0:4792` 和 `0.0.0.0:3340`，使用独立 mesh 和生成的数据，不带 Provider 或部署凭证。没有历史完整产品 Release 时明确输出 `first_release`，只演练首次安装和重复激活；查询、校验或升级失败都停止，不退到更老版本或略过升级。

## 配置部署目标

为每个目标创建 GitHub Environment `vibeface-node`、`vibeface-app`、`vibeface-www`。按需设置 Environment 的 main 分支限制和人工审核。仅验证使用不含生产凭证的 `vibeface-validation`。

每个目标配置以下 Secrets：

| Secret | 用途 |
| --- | --- |
| `DEPLOY_HOST` | SSH 主机名 |
| `DEPLOY_PORT` | SSH 端口，未设置时为 22 |
| `DEPLOY_USER` | 有 Docker、Nginx 与服务管理权限的账号；非 root 账号需对应免密 sudo |
| `DEPLOY_SSH_KEY` | 该目标的 SSH 私钥 |
| `DEPLOY_KNOWN_HOSTS` | 预先核验的完整 SSH host key，不在运行中接受未知主机 |
| `DEPLOY_CONFIG` | 目标运行配置 JSON，格式见 [`deploy/README.md`](../../deploy/README.md) |

真实主机、用户、身份、数据路径及证据不写入公共仓库。服务器需要 Docker Compose v2、Python 3 和既有 Nginx。Core 镜像使用 Ubuntu 24.04 运行时，静态镜像使用 Nginx；它们都不安装 AI Provider 或带入模型凭证。

## 保留身份并核对上线

公共节点挂载自己的既有数据目录，使用原 UID/GID。空目录、身份不符、epoch 不匹配或仍被旧节点占用时停止；不初始化新身份、不自动迁移数据。从宿主 systemd 服务迁入前，须由维护者先停旧服务、完成数据交接，并移除或改写所有引用该数据目录的 systemd unit 文件和 drop-in；脚本按文本检查这些文件，仍有引用就拒绝激活，不自动迁移或停止宿主进程。全新安装没有旧服务，校验已初始化的数据目录并备份后直接启动容器，失败时只停新容器。公共节点的初始化参数和官方限额见 [`deploy/README.md`](../../deploy/README.md)。

公共节点不提供网页文件。`node.vibeface.dev` 是指向 `84.54.3.78` 的 DNS-only A 记录，浏览器直接连接 `https://node.vibeface.dev:4792`，证书由宿主 acme.sh 申请并续期 Let's Encrypt 的域名证书，安装到数据目录后 Core 不重启即换用。部署前必须确认目标主机与提供商防火墙对公网开放 TCP 4792、UDP 4792 和 TCP 3340（TCP 80 供 acme.sh 签证书），脚本不做这一步，见 [`deploy/README.md`](../../deploy/README.md)。客户端直接连接公共节点主机上的网页接入、relay 和全网的 UDP 端口，都不经过 App 主机。

部署成功必须核对实际容器 image ID、源码和版本、原身份、网页资源以及对应角色的网络行为。容器存活或 HTTP 200 不代表全网公网 UDP 入网、relay 连接、浏览器排队接入、配对和真实任务均已验收。失败或 SSH 响应未知时先检查远端 journal 和运行状态，不直接重发部署。
