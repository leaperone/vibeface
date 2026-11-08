---
title: 发布官方服务部署
description: 使用已发布的产品制品部署官方 Discovery、Relay 与 App。
---

# 发布官方服务部署

产品制品发布和官方服务部署是两个独立操作。`release product` 发布不可变的产品制品，不更新运行中的服务。部署必须显式选择已经发布的精确产品 Tag，不能使用分支、`latest` 或未完成的 Release。

官方 Discovery、Relay 和 App 通过部署流水线消费同一产品 Release。Discovery 与 Relay 使用独立的数据目录、身份和容器，App 使用独立的静态网页容器。更换镜像不重新初始化个人网络，也不删除服务数据。

部署验收分别检查制品身份、运行版本、网页资源、Discovery 与 Relay 的真实协议。容器启动成功不能代替这些检查。生产部署凭证、主机地址与数据目录由部署环境配置，不写入仓库。
