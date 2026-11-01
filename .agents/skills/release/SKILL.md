---
name: release
description: 按应用前缀发布 Vibeface 的 www、app、core、cli 或 mobile。
---

# Vibeface release

用户必须指定应用和渠道，或明确要求五个应用一起发布。遵守仓库 AGENTS.md，使用独立 worktree 和一个 PR。读取 docs/releasing.md、scripts/release.py 和目标应用版本文件。

Nightly 格式为 X.Y.Z-nightly.YYYYMMDD.N，日期取 UTC，序号按应用同日已存在 tag 的最大序号加一。不要使用可变 nightly tag。准备版本和 CLI 的 release-lock.json；Mobile 保持纯数字 marketing version，增加原生 build number。更新 Cargo.lock。

在 PR 中完成相关检查和独立审查，按用户授权合并。未合并源码不打 tag。调用 scripts/release.py plan --app APP 验证身份，从确切已验证提交创建 APP-vVERSION。五应用首次发布先发 Core、App，确认其公开产物后发 CLI；www、mobile 独立。

发布由 release.yml 执行。打 tag 后使用 gh-run-watch 持续监控到终态，核对 release.json 的 tag、源码 SHA、文件集合和实际下载摘要。已有 release 先对账，不覆盖或移动 tag。失败后在新 PR 修复源码，再创建新版本。

npm 是 CLI 的独立分发步骤，使用 npm-publish.yml 的精确 CLI tag 产物，先 dry run 后在已有授权范围发布。第一次发布需要 npm scope 权限或可信发布配置，不索取聊天凭证。

iOS 没有签名或权限时明确报告阻塞，Android GitHub Release 不代表 TestFlight。报告各应用 tag、Actions 和实际发布结果，部署与运行验收单列。
