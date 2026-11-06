---
name: release
description: 使用统一产品 Tag 或独立官网 Tag 发布 Vibeface，核对并行构建、完整制品、npm 与部署结果。
---

# Vibeface release

## 确认发布范围

只执行用户明确授权的发布。已有会话中的授权仍适用时继续执行，不重复确认。开发完成、合并 PR、preflight 通过、版本文件变化，以及修改本 skill，都不构成发布授权。语境中的 `ship` 若指合并 PR，只完成 Shipping。

发布范围必须确定发布组、渠道与目标版本。产品组包含 Core、CLI、Web App 和 Mobile，官网组独立。精确版本可以推出渠道；Stable 的 `patch`、`minor`、`major` 必须由用户选择，不默认 patch。按该应用最新正式 tag 计算递增；尚无正式版时使用用户指定的初始版本。Nightly 沿用已确认的基准版本。缺少影响范围的信息时询问，不自动扩大为其他发布组。

示例：`release product stable 0.0.1`、`release product nightly`、`release www stable patch`。这些是 skill 参数，不是 shell 命令。

先读 [发布说明](../../../packages/docs/releasing.md)、[发布工具](../../../scripts/release.py) 和目标应用的版本文件。版本准备遵守 [AGENTS.md](../../../AGENTS.md) 的独立 worktree、单 PR 与合并规则。

| 发布组 | Tag | 版本来源 | 交付结果 |
| --- | --- | --- | --- |
| product | `v<version>` | CLI Cargo 版本；Core、Web App、Mobile 的发行版本必须与之完全一致 | 一个 Release 包含三平台电脑完整包、Core、Web App 和 Android APK |
| www | `www-v<version>` | `apps/www/package.json` | 独立官网归档，Stable 还请求部署 |

版本只使用 `X.Y.Z`、`X.Y.Z-rc.N` 或 `X.Y.Z-nightly.YYYYMMDD.N`。Nightly 日期取 UTC，N 是该发布组、基准版本和日期下远端已有序号的最大值加一。先获取远端 tags，再计算；产品与官网独立计数。产品只使用 `v`，官网只使用 `www-v`；不新增旧应用前缀别名，不使用可变 `nightly` Tag、build metadata 或重复版本。

## 每日自动 Nightly

`ci.yml` 的 main schedule 在 `ci-gate` 成功后运行 `scripts/daily-nightly.py`，手动 CI 和普通 main push 不运行它。启用须已明确授权未来每日机械版本 PR 的自动 squash，并配置单仓库 fine-grained `NIGHTLY_RELEASE_TOKEN`，授予 Contents、Pull requests、Actions 所需权限。实现合并与本 skill 变化均不代表配置或启用完成。缺 secret 时明确失败，不挪用站点部署 token，不改保护，不 fallback。

协调器从原 run.created_at 固定 UTC 日期，先恢复 `bot/daily-nightly/YYYYMMDD` 的唯一 PR、版本和真实 squash SHA。即使业务源码未变化，每日仍递增产品组和官网 Nightly；产品四应用使用同一版本，保留 marketing version、iOS buildNumber 和依赖解析结果。版本 PR 只允许机械版本字段、两个 Cargo.lock 本仓包版本、Android versionCode。PAT 只用于 PR 创建与普通立即 squash；PAT 没有路径级权限，限制由受信脚本的完整 tree、仓库、作者、base、head 校验执行。普通 GITHUB_TOKEN 处理分支、只创建的 tag 和精确 dispatch。

合并后仅实际 SHA 的 main push CI success 放行。产品内部并行构建并完整汇总后公开一个 Release，产品与官网运行可并行。产品 Nightly 自动发布 npm，Nightly 不发站、不做 iOS。失败或响应未知先对账，只从原 run 完整 artifact 重跑失败 publisher，不能重建后覆盖。npm 失败使每日协调器失败，不自动重试；对账后重跑原 release run 的失败 job，复用原 npm artifact id。artifact 丢失、过期或存在字节冲突时停止。重跑原 schedule run 即可恢复原日任务，不另建版本 PR，不重新分配同日版本。不要在人工发版会话中无授权调用此自动协调入口。

## 准备并验证版本

1. 在任务 worktree 中同步产品四应用版本及 `Cargo.lock` 的本仓包版本，或只修改独立官网版本。Mobile marketing version 和原生 build number 按目标分发规则处理。准备好的精确版本可直接发布，不为打 tag 再自动递增一次。
2. Web、三个原生平台和 Android 在同一运行并行构建。每个平台组包只等自己的原生输入和 Web，以不可变 artifact ID 验证同一源码、版本与目标。不得等待或拉取独立公开组件 Release；只有完整产品集通过才公开一个 Release。
3. 在同一 PR 完成相关检查和独立审查，再按已有授权合并。不得直接在 main 上修改、commit 或 push 版本。
4. 固定包含版本修改的完整 `SOURCE_SHA`。在指向该提交的干净 worktree 中验证；保留主 checkout 的无关修改。提交必须已进入远端 main，版本必须与 tag 完全一致。
5. 核对该 SHA 的必要检查与 main CI 均已通过。`pending`、缺少检查或只有其他 SHA 的成功记录均不能放行。失败先修复，不用打 tag 探测构建是否能过。

打 tag 前使用现有只读校验入口，`APP` 取 `product` 或 `www`，`VERSION` 取已确认的版本：

```bash
set -euo pipefail
git fetch origin main --tags
SOURCE_SHA=$(git rev-parse HEAD)
if [[ "$APP" == product ]]; then TAG="v$VERSION"; else TAG="www-v$VERSION"; fi
python3 scripts/release.py plan --app "$APP" --tag "$TAG" --require-main
gh run list --repo leaperone/vibeface --workflow ci.yml --commit "$SOURCE_SHA" \
  --json databaseId,event,headBranch,headSha,status,conclusion,url
git ls-remote --tags origin "refs/tags/$TAG" "refs/tags/$TAG^{}"
```

要求工作树干净、`plan` 成功，且 main CI 的 `headSha` 等于 `SOURCE_SHA`、`event=push`、`headBranch=main`、状态为 `completed/success`。同时核对本地与远端同名 tag；已有 tag 按下一节处理。只有本地、远端均无该 tag 且检查通过时，才创建并仅推送这一个 tag：

```bash
set -euo pipefail
test "$(git rev-parse HEAD)" = "$SOURCE_SHA"
python3 scripts/release.py plan --app "$APP" --tag "$TAG" --require-main
REMOTE_TAGS=$(git ls-remote --tags origin "refs/tags/$TAG" "refs/tags/$TAG^{}")
test -z "$REMOTE_TAGS"
git tag -a "$TAG" "$SOURCE_SHA" -m "$TAG"
git push origin "refs/tags/${TAG}:refs/tags/${TAG}"
```

GitHub 的不可变发布 Tag 规则保护产品 `v*`、官网与历史应用 Tag，禁止更新和删除，没有绕过者。创建时的完整格式由预检和 CI 验证，CI 还核对版本文件与 main 归属。规则保护不代替打 tag 前的检查。

## 处理现存 tag 与失败

- 同名本地或远端 tag 已存在：解析 annotated tag 的最终 commit 并核对 SHA。不同提交立即停止，禁止覆盖、强推、删除重建或改保护规则。同一提交的本地 tag 可直接推送；远端已存在则检查已有 Release 和关联 runs，已成功就报告已有结果。
- 相同 tag、相同源码的构建因基础设施失败时，先对账并保留已有制品，再重跑失败任务。需要重新派发时使用 `gh workflow run release.yml --repo leaperone/vibeface --ref "$TAG" -f app="$APP" -f tag="$TAG" -F publish=true`。不重新打 tag，不覆盖已上传字节；若重建结果与已上传制品不同，停止重试并使用新版本。
- 修复源码必须走新 PR 和新版本、新 tag。公开 Release 和 tag 不覆盖、不删除；回滚也通过新版本交付修正。
- 推送或发布返回结果未知时，先查询远端 tag、Release 和 Actions 对账，再决定下一步。不要直接重发。

## 跟踪实际发布结果

每次推送 tag 后立即按完整 `SOURCE_SHA`、`TAG` 和事件核对 [release.yml](../../../.github/workflows/release.yml) 的 runs。使用 `gh-run-watch` 持续跟踪所有本次关联 runs 到终态。不要用“最新一条”猜 run，也不要把 API 观察超时当作构建失败后重复发布。

发布成功后，核对唯一 `release.json` 的发布组、Tag、版本、完整源码 SHA、精确文件集合和 SHA-256。产品必须包含 17 个载荷文件；Core、App 与官网核对 `COMPONENT-INFO.json`，完整电脑包核对九字段 `BUILD-INFO.json` 和内部文件摘要，APK 核对 `.provenance.json`。所有产品载荷必须来自同一源码和版本。上传成功不等于制品通过运行验收。

App 的公网入口是 `https://app.vibeface.dev`。Stable 产品 Tag 的完整 Release 发布成功后自动请求站点部署，RC 和 Nightly 只生成归档；部署后须核对线上资源的源码身份、SPA 路由、API 404 和首次连接页面。

www Stable 还要跟踪 `leaperone/leaperone-releases` 的 `deploy-vibeface-www.yml`。该 workflow 的标题包含 `src=<SOURCE_SHA>`，用它匹配源码；其 `headSha` 属于部署仓库，不能拿来与 Vibeface 源码比较。确认下游部署成功后再检查 `www.vibeface.dev` 的实际页面。源码仓库 dispatch 成功不代表网站已经上线。

npm 使用 [npm-publish.yml](../../../.github/workflows/npm-publish.yml)，当前仅支持 CLI Nightly。`release.yml` 在 产品 `publish` 成功后自动调用它，以 `collect` 产生的原不可变 npm artifact ID 验证源码与包，再通过 OIDC 公开发布。npm trust 须分别允许调用方 `release.yml` 与手动入口 `npm-publish.yml`，均绑定 `npm-release` environment。手动入口默认 dry run，实际发布须显式 `publish=true`，不得配置长期 npm token 或索取聊天凭证。配置命令和 registry 对账规则见 [发布说明](../../../packages/docs/releasing.md#npm-nightly-制品与发布)；仓库改动不代表远端 trust 已生效或真实 OIDC 发布已验收。

Mobile 的 TestFlight、App Store、OTA 和真机安装不由当前 tag 流程完成。按用户明确指定的平台另行处理，不能把 Android APK 或 CI 成功报告成 iOS 已交付。

最终逐发布组报告 Tag、完整源码 SHA、Actions URL 与结论、Release 与每类载荷的下载校验结果。部署、npm、移动端分发和实际运行验收分别说明；未发生或未验证的步骤明确标注。

## 构建验证与切换

手动 `release.yml` 默认 `publish=false`，可从开发分支执行产品或官网完整构建，不要求真实 Tag。该结果只证明构建与验收，不代表公开发布、部署、npm registry 写入或升级已发生。使用 `gh workflow run release.yml --ref <branch> -f app=product -F publish=false` 验证实际并行与最终完整 artifact。

真正发布必须使用现存精确 Tag 作为 workflow ref，显式 `publish=true`；身份检查还要求该源码的 main push CI success。只有 `publish` 任务能写 GitHub Release，所有生产者和汇总任务只上传 Actions 制品。

旧 CLI 只识别旧 `cli-v` 格式，首次切换按安装说明手动安装新版完整包，之后由新版跟踪产品 `v`。不创建兼容 Tag、不自动覆盖已有安装或迁移数据。
