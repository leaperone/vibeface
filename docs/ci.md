# 按改动选择 CI

`.github/workflows/ci.yml` 使用 `scripts/ci.py` 统一选择检查范围、Rust 测试分组和必需任务。工作流没有顶层 `paths` 筛选；每次触发都生成计划并执行 `ci-gate`。

| 改动 | PR 中执行的检查 |
| --- | --- |
| README、项目指引、`docs/`、SDK README | 静态快检，不构建或运行 Rust 测试 |
| `crates/node/web/`、Web 构建脚本 | 浏览器密码学、Web 测试与类型检查、静态构建、Linux x64 完整包及网页进程验收 |
| Node Rust、迁移、SDK、安装、打包或进程验收脚本 | Node 检查、测试和制品验收 |
| Bridge | Bridge 和 Node |
| Protocol | Protocol、Bridge 和 Node |
| Secure、密码学验收脚本 | Secure、Node 和浏览器密码学互通验收 |
| Cargo manifest、锁文件、工具链、CI 规则、未知输入 | 全工作区 |

Web 路径先于 Node 路径分类。Web-only PR 不运行 Node 库的同步、存储、调度测试，但仍构建真正的 Node 制品，用 `verify-node.py --web-only` 启动独立进程检查资源、SPA 路由、CSP、缓存与 404。Node 依赖 Protocol、Bridge、Secure，因此这些库变更会沿反向依赖选中 Node。

PR 使用 `merge-base(base, head)..head` 的完整差异。删除与重命名同时考虑旧路径和新路径，不只判断最后一个提交。无法识别的输入按全量处理。

| 场景 | 测试与制品范围 |
| --- | --- |
| Draft PR | 受影响 crate 的快速测试；需要制品时仅构建 Linux x64 |
| Ready PR | 受影响 crate 的快、慢测试；Node 改动构建三个平台；Web-only 保留 Linux x64 |
| `main` 代码 push | 全工作区、慢测试和三平台制品；纯文档提交只做快检 |
| 定时、手动、merge queue、版本 tag | 强制全量 |

`ready_for_review`、`converted_to_draft`、`synchronize` 和 PR base 调整会重新触发。Draft 的绿色 gate 仅表示草稿阶段应运行的检查通过；Draft PR 本身不能合并。正式 PR 后续提交会重新执行其完整受影响检查。

只改 PR 标题或描述时跳过任务，并使用独立的并发组与检查名称，保留正在运行的代码检查和必需的 `ci-gate` 结果。

`rust-fast`、`rust-slow` 与 `rust-check` 独立运行。慢测试名单集中在 `scripts/ci.py` 的 `SLOW_TESTS`，包含七条重型用例，覆盖 NET-1、真实 Noise 同步和队列饱和场景。快测试跳过这些用例，慢测试按完整名称精确选择。运行前用 `cargo test -- --list` 验证每个慢测试恰好存在一次，并拒绝会多匹配的过滤器，避免重命名后出现零测试成功。正式全量覆盖所有默认测试；原本 `ignored` 的用例仍由 Rust 默认忽略。

`ci-gate` 始终执行，要求计划中的每个任务成功。必需任务失败、取消、缺失或意外跳过都会使 gate 失败。GitHub 仓库还需将 `ci-gate` 设为必需检查，并要求更新到最新 base 或使用 merge queue，才能强制阻止检查未通过的合并；`main` 或定时全量结果不能替代本次 PR 的合并前检查。

本地 Rust preflight 执行快测试和 Clippy。慢测试由正式 PR 的必需 `ci-gate` 检查，避免合并前在本机重复执行同一组重型场景。

远端慢组使用 `opt-level=1` 的测试构建，并保留调试断言与整数溢出检查；它使用独立缓存，不改变用例或夹具规模。

推送版本 tag 才触发发布，手动全量检查只生成制品。版本 tag 必须与 Cargo 版本完全一致且已经进入 `main`。只有全量 gate 与三平台制品成功后才运行发布；SHA256、源提交、存储 epoch、资产清单和下载比对规则保留。

Nightly 版本的 Ready Node PR 和全量场景还会将三个已验收的原生包组装成 npm 包，在三个平台安装并运行进程烟测。Web-only 和 Draft PR 不组装全平台 npm 包。`npm-package` 和 `npm-smoke` 属于相应计划的必需检查，Nightly tag 必须等这些检查通过才能发布。

本地检查选择规则，无需编译：

```sh
python3 scripts/ci.py --self-test
python3 scripts/ci.py plan --base origin/main --head HEAD --event pull_request
actionlint .github/workflows/ci.yml
```

需要运行选定 crate 的测试时：

```sh
python3 scripts/ci.py test --tier fast --packages vibeface-bridge vibeface
python3 scripts/ci.py test --tier slow --packages vibeface
```

这种拆分减少无关覆盖和草稿阶段的重复重型测试，不改变慢测试内部的计算量。Node 正式 PR 仍需等待相应慢测试完成。模型调用、实体手机和真实公网拓扑不在这些检查的覆盖内。
