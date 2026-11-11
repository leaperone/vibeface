---
name: release
description: 使用统一产品 Tag 或独立官网 Tag 发布 Vibeface，构建并签名本机 iOS 真机版本，核对制品与 npm 发布结果。
---

# Vibeface release

本 skill 及引用文档只保留通用流程和占位变量。个人账号、设备名称或 UDID、证书身份或指纹、本机绝对路径、凭据和内部会话信息仅在运行时读取并按需留在私有本地证据中，不写入 skill 或公开 PR，凭据不写入日志。

## 本机 iOS 构建

`release ios <SOURCE_SHA>` 构建指定源码的本机开发签名 App。执行前核对该源码声明的版本；它不创建独立 iOS Tag，也不发布整套产品。先读 [iOS 命令行构建](references/ios-local-build.md)。默认由独立 Aqua LaunchAgent 执行 Expo、CocoaPods 和 `xcodebuild`，复用本机已有开发签名身份，不依赖操作 Xcode 窗口。当前 Codex 后台 daemon 保持原样。

本机开发签名构建、真机安装、公开产品发布和 TestFlight 分发分别核验、分别授权。只要求本机构建时，不因此创建产品 Tag 或发布 npm。安装已有制品使用 [Deploy skill](../deploy/SKILL.md) 的 `deploy ios <制品路径>`；缺少可安装制品时先完成单独授权的构建。CI 的分发签名仍使用现有 `scripts/build-ios.sh` 流程。

## 确认发布范围

只执行用户明确授权的发布。已有会话中的授权仍适用时继续执行，不重复确认。开发完成、合并 PR、preflight 通过、版本文件变化，以及修改本 skill，都不构成发布授权。语境中的 `ship` 若指合并 PR，只完成 Shipping。

发布范围必须确定发布组、渠道与目标版本。产品组包含 Core、CLI、Web App、Desktop 和 Mobile，官网组独立。精确版本可以推出渠道；Stable 的 `patch`、`minor`、`major` 必须由用户选择，不默认 patch。按该应用最新正式 tag 计算递增；尚无正式版时使用用户指定的初始版本。Nightly 沿用已确认的基准版本。缺少影响范围的信息时询问，不自动扩大为其他发布组。

示例：`release product stable 0.0.1`、`release product nightly`、`release www stable patch`。这些是 skill 参数，不是 shell 命令。

先读 [发布说明](../../../packages/docs/releasing.md)、[发布工具](../../../scripts/release.py) 和目标应用的版本文件。人工版本准备遵守 [AGENTS.md](../../../AGENTS.md) 的独立 worktree、单 PR 与合并规则。

| 发布组 | Tag | 版本来源 | 交付结果 |
| --- | --- | --- | --- |
| product | `v<version>` | CLI Cargo 版本；各产品组件的发行版本必须与之完全一致 | 一个 Release 包含四个原生目标的电脑完整包、macOS/Windows/Linux 桌面 App、Web App、Android APK、iOS 设备归档及配置签名后的 IPA |
| www | `www-v<version>` | `apps/www/package.json` | 独立官网归档，不触发部署 |

版本只使用 `X.Y.Z`、`X.Y.Z-rc.N` 或 `X.Y.Z-nightly.YYYYMMDD.N`。Nightly 日期取 UTC，N 是该发布组、基准版本和日期下远端已有序号的最大值加一。先获取远端 tags，再计算；产品与官网独立计数。产品只使用 `v`，官网只使用 `www-v`；不新增旧应用前缀别名，不使用可变 `nightly` Tag、build metadata 或重复版本。

## 每 4 小时自动 Nightly

`ci.yml` 保留 `23 */4 * * *`，UTC 槽位为 00:23、04:23、08:23、12:23、16:23、20:23。GitHub schedule 是 best effort，可能延迟或跳过，不保证每槽运行。仅 main schedule 的 `ci-gate` 成功后运行 `scripts/daily-nightly.py`；手动 CI 和普通 main push 不自动发布 Nightly。

协调器只用 `GITHUB_TOKEN`，不需要 PAT，不创建版本分支或 PR，不合入 main。以原 schedule run 的精确 main SHA 作唯一 parent，生成普通机械版本提交，只改七个版本文件中的发行版本、Cargo.lock 三个本仓包版本及 Android、iOS 构建号，保留 marketing version、依赖和其他内容。共享验证器对比完整 tree，并运行 `cargo metadata --locked --no-deps` 核对 Core、CLI、Desktop 三个本仓包的清单版本（这条命令带 `--no-deps` 时不校验锁文件），不以路径白名单放行。

原 run 的 UTC `created_at` 向下取整到四小时批次，版本日期仍取该 run 的 `YYYYMMDD`。提交记录原 schedule run ID、批次和上一可信产品 Nightly 来源。移动构建号从 main 与上一可信 Nightly 取数值上界后加一；iOS 按数字分段比较，末段溢出时停止，不回绕。协调器全局串行。同批次、延迟或跨日恢复保持原版本、构建号、提交与 artifact；上一来源完整发布且 npm 成功、main 无新提交时跳过。部分 Tag、Release 或 npm 必须先恢复原来源，不新建版本覆盖。

所有发布入口统一验证 main 来源或精确机械 Nightly。detached 提交仅允许 Nightly；Stable/RC 仍要求 main。机械 Nightly 的 CI 证明必须是原 schedule run 的精确仓库、workflow path、main、parent SHA 和成功 `ci-gate`，不等待包含协调 job 的整个 run success。正常 main Tag 仍要求同一 SHA 的 main push CI success。Tag、制品与 provenance 的 source 始终是实际版本提交，不替换为 parent。

产品与官网并行发布，产品最终完整清单和 npm 分别核对。Tag 只创建，冲突立即停止；未知响应先读远端对账，不重复写入。仅从原运行的完整不可变 artifact 重跑失败 publisher；原产品和 npm artifact 完整时可恢复原 npm 发布。缺失、过期或字节冲突时停止。自动 Nightly 不部署、不上传 TestFlight 或 App Store；仍构建设备归档，签名配置完整时导出 IPA。不要在人工会话中无授权调用自动协调入口。

## 准备并验证版本

1. 在任务 worktree 中同步各产品组件版本及 `Cargo.lock` 的本仓包版本，或只修改独立官网版本。Mobile marketing version 和原生 build number 按目标分发规则处理。准备好的精确版本可直接发布，不为打 tag 再自动递增一次。
2. Web、macOS、Windows、Linux x64、Linux ARM64 原生程序、Android 和 iOS 在同一运行并行构建。桌面 App 只等待本平台完整运行包。每个平台组包只等自己的原生输入和 Web，以不可变 artifact ID 验证同一源码、版本与目标。不得等待或拉取独立公开组件 Release。每组校验通过即可追加并供手动下载；完整产品集通过才追加全局 `release.json`，标记整套发布完成。
3. 在同一 PR 完成相关检查和独立审查，再按已有授权合并。不得直接在 main 上修改、commit 或 push 版本。
4. 固定包含版本修改的完整 `SOURCE_SHA`。在指向该提交的干净 worktree 中验证；保留主 checkout 的无关修改。人工准备的提交必须已进入远端 main，版本必须与 tag 完全一致；自动机械 Nightly 使用前述共享来源门。
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

GitHub 的不可变发布 Tag 规则保护产品 `v*`、官网与历史应用 Tag，禁止更新和删除，没有绕过者。创建时的完整格式由预检和 CI 验证，CI 还核对版本文件与共享来源规则。规则保护不代替打 tag 前的检查。

## 处理现存 tag 与失败

- 同名本地或远端 tag 已存在：解析 annotated tag 的最终 commit 并核对 SHA。不同提交立即停止，禁止覆盖、强推、删除重建或改保护规则。同一提交的本地 tag 可直接推送；远端已存在则检查已有 Release 和关联 runs，已成功就报告已有结果。
- 相同 tag、相同源码的构建因基础设施失败时，先对账并保留已有制品，再重跑失败任务。需要重新派发时使用 `gh workflow run release.yml --repo leaperone/vibeface --ref "$TAG" -f app="$APP" -f tag="$TAG" -F publish=true`。自动 Nightly 的 tag 不重新派发：协调器要求每个 tag 只有一个 release run，出现第二个 run 就判定该来源未完成并停止，只重跑原 run 的失败 job。不重新打 tag，不覆盖已上传字节；若重建结果与已上传制品不同，停止重试并使用新版本。
- 修复源码必须走新 PR 和新版本、新 tag。构建中的同一 Release 允许追加其他已校验平台的附件；已公开字节和 tag 不覆盖、不删除。回滚也通过新版本交付修正。
- 推送或发布返回结果未知时，先查询远端 tag、Release 和 Actions 对账，再决定下一步。不要直接重发。

## 跟踪实际发布结果

每次推送 tag 后立即按完整 `SOURCE_SHA`、`TAG` 和事件核对 [release.yml](../../../.github/workflows/release.yml) 的 runs。使用 `gh-run-watch` 持续跟踪所有本次关联 runs 到终态。不要用“最新一条”猜 run，也不要把 API 观察超时当作构建失败后重复发布。

公开附件不代表整套发布已完成。缺少全局 `release.json` 时，报告为构建中，用户可手动下载已校验平台；新版安装器和自动更新器跳过它。产品 run 失败且尚无 `release.json` 时，`release-failed` 把正文标为 FAILED；重跑失败 job 完成后写回完成说明，完整重跑先恢复为构建中。已安装旧客户端无法追溯修改，可能更新已具备完整电脑包和校验文件的平台，缺少目标时失败并保留现版。完整发布成功后，核对唯一 `release.json` 的发布组、Tag、版本、完整源码 SHA、精确文件集合和 SHA-256。产品文件集合必须与 `scripts/release.py` 定义的完整集合一致，包含所有桌面安装包、移动端包、校验和与来源收据；Core、App 与官网核对 `COMPONENT-INFO.json`，完整电脑包核对九字段 `BUILD-INFO.json` 和内部文件摘要，APK 核对 `.provenance.json`。所有产品载荷必须来自同一源码和版本。上传成功不等于制品通过运行验收。

产品发布不更新 App 或公共节点。用户要求部署这些服务时，使用相邻的 [deploy skill](../deploy/SKILL.md)，显式选择已经完整发布的产品 Tag。不得由 `release product` 自动派发部署。

官网发布同样不自动部署。部署官方站点时，在本仓库执行 `deploy www <精确官网 Tag>`，与产品服务使用相同的独立部署流水线。核对实际运行版本、镜像和线上页面，不能把 workflow 启动当作上线。

npm 使用 [npm-publish.yml](../../../.github/workflows/npm-publish.yml)，当前仅支持 CLI Nightly。`release.yml` 在产品 `publish-product` 成功后自动调用它，以 `collect` 产生的原不可变 npm artifact ID 验证源码与包，再通过 OIDC 公开发布。npm trust 须分别允许调用方 `release.yml` 与手动入口 `npm-publish.yml`，均绑定 `npm-release` environment。手动入口允许同 Tag、同 SHA 的唯一已完成 release run，只要 `collect` 和 `publish-product` 成功，即使总 run 因 npm 标红。手动入口默认 dry run，实际发布须显式 `publish=true`，不得配置长期 npm token 或索取聊天凭证。配置命令和 registry 对账规则见 [发布说明](../../../packages/docs/releasing.md#npm-nightly-制品与发布)；仓库改动不代表远端 trust 已生效或真实 OIDC 发布已验收。

iOS 默认使用 `app-store-connect` 分发方式，先通过 TestFlight 内测，再准备 App Store 发布。签名 IPA 的 `testflight_eligible=true` 只代表导出方式和描述文件通过校验，不代表 Apple 已接收或内测已可安装。TestFlight 上传、Apple 处理、内部测试组分配、App Store 提交和真机验收另行完成；不能把未签名设备归档或 CI 成功报告成可安装的 iOS App。

最终逐发布组报告 Tag、完整源码 SHA、Actions URL 与结论、Release 与每类载荷的下载校验结果。部署、npm、移动端分发和实际运行验收分别说明；未发生或未验证的步骤明确标注。

## 构建验证与切换

手动 `release.yml` 默认 `publish=false`，可从开发分支执行产品或官网完整构建，不要求真实 Tag。该结果只证明构建与验收，不代表公开发布、部署、npm registry 写入或升级已发生。使用 `gh workflow run release.yml --ref <branch> -f app=product -F publish=false` 验证实际并行与最终完整 artifact。

真正发布必须使用现存精确 Tag 作为 workflow ref，显式 `publish=true`；身份检查对 main 来源要求同一 SHA 的 main push CI success，对机械 Nightly 要求原 schedule parent 的成功 `ci-gate`。`release-start` 只负责创建或确认一个公开 Release，各平台发布任务仅写自己独占的附件名称，最终 `publish-product` 最后写入全局 `release.json` 和完成状态；生产者与 `collect` 仍只上传 Actions 制品。

旧 CLI 只识别旧 `cli-v` 格式，首次切换按安装说明手动安装新版完整包，之后由新版跟踪产品 `v`。不创建兼容 Tag、不自动覆盖已有安装或迁移数据。
