---
name: deploy
description: 部署已验证制品。通过 CI/CD 部署官方公共节点、App 或 WWW，或将已签名 iOS App 原位安装到指定 iPhone；不构建或发布新制品。
---

# 部署已验证制品

构建、签名和发布制品由 [Release skill](../release/SKILL.md) 负责，本入口消费已有制品。以下形式均为 skill 参数，不是新增的系统 CLI 子命令。个人账号、设备或证书标识、本机路径和凭据不写入 skill 或公开 PR；运行参数与必要回执只在私有本地使用，凭据不写入日志。

## 安装到 iPhone

命令形式为 `deploy ios <制品路径>`，目标为已完成来源和签名验收的 iPhoneOS `.app`。先核对用户授权的设备和版本；缺少制品时报告缺口并转交 `release ios <SOURCE_SHA>`，不在部署过程中隐式重新构建、签名或发布产品。

- 读取制品来源回执，核对源码 SHA、版本、build number、文件摘要和实际路径。复验 `codesign --verify --deep --strict`、实际签名身份、entitlements、embedded profile 与 iPhoneOS 平台；profile 须包含目标设备且仍有效。
- 用精确 UDID 执行 `devicectl device install app` 原位安装，再独立读回版本、启动 App 并查询运行进程。不卸载、不清数据或钥匙串；保留本次安装和启动回执。
- 安装结果未知时先查询设备上的版本与进程，不把失败当成功，也不直接重发安装命令。安装、进程启动、界面验收和数据完整性分别报告，文件元数据相同不能代替完整数据校验。
- App Store Connect 签名 IPA、未签名设备归档和模拟器制品不属于此直接安装入口。TestFlight 上传、Apple 处理和测试组分配另行明确授权，不能把本地安装报告为 TestFlight 已交付。

## 部署官方服务

命令形式为 `deploy <all|node|app|www> <精确已发布 Tag>`。缺少目标或 Tag 时先查询已发布版本，不能默认选择最新版本、创建新 Tag 或重新编译产品。

先阅读[服务部署说明](../../../packages/docs/service-deployments.md)，再执行：

```sh
python3 scripts/deploy.py plan --service '<service>' --tag '<tag>'
```

核对已授权的目标与版本后，执行独立部署发布：

```sh
python3 scripts/deploy.py release --service '<service>' --tag '<tag>'
```

该命令请求 CI/CD，不直接修改服务器。跟踪本仓库 `deploy-services.yml` 对应 Tag、源码 SHA 和目标的运行到终态。使用 `gh-run-watch` 持续监控。派发成功不代表部署成功。

验收必须包括镜像 digest、实际运行版本和原身份，以及该角色的真实网络行为。`node` 是香港的官方公共节点，同时提供发现服务、开放 relay 和浏览器接入，镜像不含网页资源；`app` 只提供静态网页，浏览器直接连该节点的 `gateway_url`（`https://node.vibeface.dev:4792`，DNS-only A 记录指向 `8.217.72.118`，不开 Cloudflare 代理）。失败或响应未知时先对账，不能直接重发、重建产品或切回不明版本。
