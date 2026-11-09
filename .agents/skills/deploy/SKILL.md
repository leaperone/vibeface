---
name: deploy
description: 使用已发布产品 Tag，通过 CI/CD 部署官方公共节点、App 或 WWW。用于 deploy、部署上线或发布服务部署，不发布产品制品。
---

# 部署官方服务

产品制品发布由 `release` 负责，服务部署使用本入口。命令形式为 `deploy <all|node|app|www> <精确已发布 Tag>`。缺少目标或 Tag 时先查询已发布版本，不能默认选择最新版本、创建新 Tag 或重新编译产品。

先阅读[服务部署说明](../../../packages/docs/service-deployments.md)，再执行：

```sh
python3 scripts/deploy.py plan --service '<service>' --tag '<tag>'
```

核对已授权的目标与版本后，执行独立部署发布：

```sh
python3 scripts/deploy.py release --service '<service>' --tag '<tag>'
```

该命令请求 CI/CD，不直接修改服务器。跟踪本仓库 `deploy-services.yml` 对应 Tag、源码 SHA 和目标的运行到终态。使用 `gh-run-watch` 持续监控。派发成功不代表部署成功。

验收必须包括镜像 digest、实际运行版本和原身份，以及该角色的真实网络行为。`node` 是香港的官方公共节点，同时提供发现服务、开放 relay 和浏览器接入，后两者经 Cloudflare 代理在 `https://node.vibeface.dev`，镜像不含网页资源；`app` 只提供静态网页，浏览器直接连该节点的 `gateway_url`。失败或响应未知时先对账，不能直接重发、重建产品或切回不明版本。
