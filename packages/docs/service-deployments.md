---
title: 发布官方服务部署
description: 使用已发布的产品制品部署官方 Discovery、Relay 与 App。
---

# 发布官方服务部署

产品制品发布和官方服务部署是两个独立操作。`release product` 发布不可变的产品制品，不更新运行中的服务。部署必须显式选择已经发布的精确产品 Tag，不能使用分支、`latest` 或未完成的 Release。

官方 Discovery、Relay 和 App 通过部署流水线消费同一产品 Release。Discovery 与 Relay 使用独立的数据目录、身份和容器，App 使用独立的静态网页容器。更换镜像不重新初始化个人网络，也不删除服务数据。

部署验收分别检查制品身份、运行版本、网页资源、Discovery 与 Relay 的真实协议。容器启动成功不能代替这些检查。生产部署凭证、主机地址与数据目录由部署环境配置，不写入仓库。

## 选择并部署已发布版本

先检查指定版本和服务，下面的命令只读：

```sh
python3 scripts/deploy.py plan --service all --tag v0.0.1-nightly.YYYYMMDD.N
```

将示例 Tag 替换成真实已发布产品 Tag，再执行部署发布：

```sh
python3 scripts/deploy.py release --service all --tag v0.0.1-nightly.YYYYMMDD.N
```

`--service` 接受 `all`、`discovery`、`relay` 或 `app`。支持精确的 Stable、RC 和 Nightly 产品 Tag，不自动跟随任何渠道。命令触发 `deploy-services.yml`，校验最终 `release.json` 后向部署仓库发出带有目标、Tag 和源码 SHA 的请求。发布尚未完成、元数据不匹配或缺少制品时拒绝部署。

Relay 镜像不包含网页文件；客户端接入和中继 API 保留。国内 Relay 入口同样不部署前端。网页统一由独立 App 容器提供。

源仓库请求成功后仍须核对下游部署运行，不能把派发成功当作服务上线。每个服务保留自己的身份和持久化目录；部署不初始化新身份、不升级数据库格式。
