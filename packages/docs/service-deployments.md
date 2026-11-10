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

| 目标 | 交付内容 |
| --- | --- |
| `all` | 官方公共节点和 App，不含官网 |
| `node` | 香港官方公共节点的 Core 容器：全网 bootstrap 节点、开放 relay 和浏览器接入，各有限额，不含前端文件 |
| `app` | 独立网页静态容器，只提供静态文件；网页直接连公共节点的 `gateway_url` |
| `www` | 独立官网静态容器，使用精确 `www-v<version>` Tag |

产品目标接受精确 Stable、RC、Nightly `v<version>`，官网只接受对应 `www-v<version>`。不自动跟随渠道。所有目标使用同一个 `deploy-services.yml`。部署命令以 `apply=true` 启动流水线；UI 手动运行默认 `apply=false`，只验证制品、构建容器并冒烟，不接触服务器。

```sh
python3 scripts/deploy.py release --service www --tag '<WEBSITE_TAG>'
gh workflow run deploy-services.yml --ref '<BRANCH>' -f service=app -f tag='<PRODUCT_TAG>' -F apply=false
```

真实激活仅允许从 main 执行。每次运行重新核对最终 `release.json` 的 Tag、源码 SHA、main 归属、附件集合和摘要，再下载需要的组件并核对实际字节与包内来源。容器只包装这些已发布字节，通过 SSH 传输并按 image ID 激活，无需共享镜像仓库。

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

公共节点挂载自己的既有数据目录，使用原 UID/GID。空目录、身份不符、epoch 不匹配或仍被旧节点占用时停止；不初始化新身份、不自动迁移数据。从宿主 systemd 服务迁入前，须由维护者先停旧服务并完成数据交接；脚本不自动迁移或停止宿主进程。全新安装没有旧服务，校验已初始化的数据目录并备份后直接启动容器，失败时只停新容器。公共节点的初始化参数和官方限额见 [`deploy/README.md`](../../deploy/README.md)。

公共节点不提供网页文件。`node.vibeface.dev` 是指向 `8.217.72.118` 的 DNS-only A 记录，浏览器直接连接 `https://node.vibeface.dev:4792`，证书由宿主 acme.sh 申请并续期 Let's Encrypt 的域名证书，安装到数据目录后 Core 不重启即换用。部署前必须在阿里云安全组对公网开放 TCP 4792、UDP 4792 和 TCP 3340（TCP 80 供 acme.sh 签证书），脚本不做这一步，见 [`deploy/README.md`](../../deploy/README.md)。客户端直接连接公共节点主机上的网页接入、relay 和全网的 UDP 端口，都不经过 App 主机。

部署成功必须核对实际容器 image ID、源码和版本、原身份、网页资源以及对应角色的网络行为。容器存活或 HTTP 200 不代表全网公网 UDP 入网、relay 连接、浏览器排队接入、配对和真实任务均已验收。失败或 SSH 响应未知时先检查远端 journal 和运行状态，不直接重发部署。
