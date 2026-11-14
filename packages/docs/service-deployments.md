---
title: 官方服务部署
description: 用已完成的独立 Core、App 或 WWW Release 更新对应官方服务。
---

# 官方服务部署

Core、Web App 和 WWW 完整发布后，各自的 Release workflow 自动从 main 派发对应部署并等待终态。部署只使用精确 Tag、最终 `release.json` 和校验过的资产，不重新编译产品，不选择未完成版本或可变 `latest`。

| 目标 | Tag | 部署入口 |
| --- | --- | --- |
| `node` | `core-v<version>` | `deploy-node.yml`，只更新官方公共 Node |
| `app` | `app-v<version>` | `deploy-app.yml`，只更新静态 Web App |
| `www` | `www-v<version>` | `deploy-www.yml`，只更新官网 |

各服务版本独立，不再提供 `all` 或 Product 批次。手动补部署时先检查计划：

```sh
python3 scripts/deploy.py plan --service node --tag 'core-v<version>'
python3 scripts/deploy.py release --service node --tag 'core-v<version>'
```

将目标与 Tag 前缀一起改为 `app` 或 `www` 可部署对应服务。CLI 的 `release` 派发 `apply=true`；UI 手动运行默认 `apply=false`，会验证、构建容器并演练，不触碰主机。只有 main 能执行生产激活。每个入口固定目标，共用 `deploy-services.yml` 的全局激活锁，后续请求不会取消已在进行的激活。

Node 使用 Core 的 x86_64 Linux 归档。部署前先在 runner 练习上一份完整 Core 到候选版的升级，确认原身份和数据保留，再请求线上 Node 的维护租约、备份与激活。失败遵守原有 journal/回滚流程，不以程序回退代替业务数据恢复。

App/WWW 消费各自静态 component，校验文件摘要与 CSP 后包装为静态容器。App 浏览器直接连接官方 Node 网关，不由 App 容器转发 Node API。线上配置、SSH 身份和地址保存在部署 environments，不写入发布制品。

核对对应 Actions 的最终结果、容器镜像摘要、实际版本和健康状态。Node 还需确认设备身份与浏览器接入/relay 行为，App/WWW 需读回 HTTPS 状态、静态页面和版本头。派发成功或上传完成不等于上线成功。请求结果未知时先按精确 Tag 与执行意图对账，再决定是否重跑。

容器、TLS、主机目录权限和维护流程见[部署配置](deployment.md)与仓库 `deploy/README.md`。
