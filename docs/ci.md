# 按改动选择 CI

`.github/workflows/ci.yml` 使用 `scripts/ci.py` 选择检查范围、Rust 测试分组和必需任务。每次触发都生成计划，`ci-gate` 要求选中的任务全部成功。没有顶层 `paths` 筛选。

## 每个 PR 的核心检查

Draft 与 Ready 使用相同规则。`main` push 也按路径运行核心检查；merge queue 无路径筛选，执行整个工作区的核心检查。

| 改动 | 核心检查 |
| --- | --- |
| README、项目指引、`docs/`、SDK README | 静态快检，不构建或运行 Rust 测试 |
| `crates/node/web/`、Web 构建脚本 | 浏览器密码学互通、Web 测试与类型检查、静态构建、Linux x64 完整包及网页进程验收 |
| Node Rust、迁移、SDK、安装、打包或进程验收脚本 | Node 快速测试、Clippy、Web 和 Linux x64 制品启动验收 |
| Bridge | Bridge 和 Node 的核心检查 |
| Protocol | Protocol、Bridge 和 Node 的核心检查 |
| Secure、密码学验收脚本 | Secure、Node 和浏览器密码学互通 |
| Cargo manifest、锁文件、工具链、CI 规则、未知输入 | 整个工作区的核心检查 |

静态快检保留 Rust fmt、工作流与 shell 检查、JavaScript 语法、选择器和安装器自检。涉及 Rust 时校验 Atlas 迁移文件，不打开数据库。快速 Rust 测试保留权限、个人网络隔离、数据持久化、写操作去重与未知结果对账等已有回归，只跳过 `SLOW_TESTS` 明确列出的重型用例，目前是 10 万事件追加测试。

Web 路径优先于 Node 路径。Node 对 Protocol、Bridge、Secure 的依赖用于向上扩展检查范围。PR 按 `merge-base(base, head)..head` 的完整差异选取，删除与重命名包括旧、新路径。无法识别的输入选择整个工作区核心检查。

Linux 制品核对 checksum、源码提交、版本、target、WASM 和存储 epoch，再以 `verify-node.py --web-only` 启动真实进程，检查网页资源、SPA 路由、CSP、缓存与 404。它不执行完整多节点业务验收，也不验证浏览器渲染。

## 定期、手动和发布检查

每日 UTC 03:23 定时、Actions 中手动运行 `ci`、`v*` tag 强制执行扩展检查，不受路径影响。手动入口只验证和生成制品，不发布。在 GitHub Actions 选择 `ci` → `Run workflow`，选择待验收分支后运行。

| 扩展检查 | 执行范围 |
| --- | --- |
| Rust 快、慢测试及 Clippy | 整个工作区；慢组为精确名称的 `SLOW_TESTS` |
| 最低 Rust 版本 | 安装 Cargo 声明的 MSRV，检查整个工作区 |
| 原生 release 制品 | Linux x64、Linux ARM64、macOS ARM64，分别校验身份并启动网页进程 |
| 单 Node 调度与事件日志验收 | Linux x64 一次，使用 `--phase1`；多 Node 全量验收在统一网络第三期恢复 |
| 聊天 Agent 模拟协议验收 | macOS ARM64 一次，使用 `--chat-agent` |
| Nightly npm 包 | 组装三个原生包，三个平台分别安装、检查版本并执行 `--web-only` 冒烟 |

慢组使用 `opt-level=1`，保留调试断言和整数溢出检查，使用独立缓存。快、慢组运行前校验每个慢测试名恰好匹配一个已有测试。慢组用 `--include-ignored` 运行列出的用例，其他 `ignored` 用例仍默认忽略。NET-1 七个进程专项不自动加入每日检查。

CI 不使用真实 AI Provider 或模型密钥。`--chat-agent` 使用受控 SDK 和 App Server 夹具；`--skip-codex` 跳过本机真实 Codex 协议检查。模拟协议通过不能证明真实模型理解需求或正确选择工具。

只有版本 tag 可以发布。tag 必须与 Cargo 版本一致且提交已进入 `main`；扩展 gate、三平台制品及适用的 npm 检查必须通过。发布前继续校验资产清单并下载逐字节比对，详见 [构建与发布](releasing.md)。

## 本地按改动补充验收

开发者修改同步、恢复、调度或工具执行时，除 PR 核心检查外，必须执行对应专项并记录命令及结果；不能只依赖下次定期运行。每次只跑相关模式。安装器或平台交付改动应额外手动触发 `ci`，确认目标平台制品。

无需编译即可验证 CI 规则：

```sh
python3 scripts/ci.py --self-test
python3 scripts/ci.py plan --base origin/main --head HEAD --event pull_request
actionlint .github/workflows/ci.yml
```

Rust 快、慢组可独立运行。Clippy 默认不安装 MSRV，需要时显式提供：

```sh
python3 scripts/ci.py test --tier fast --packages vibeface-bridge vibeface
python3 scripts/ci.py test --tier slow --packages vibeface
python3 scripts/ci.py check --packages vibeface
python3 scripts/ci.py check --packages vibeface --msrv 1.91
```

MSRV 参数应使用当前 `Cargo.toml` 的 `rust-version`。进程专项需要 Python 3、Node.js、当前提交构建的 Node 程序和真实 JS/WASM 密码学资源。先按 [源码构建步骤](releasing.md#ci-验证范围) 生成资源和制品，然后设置以下路径：

```sh
binary=/absolute/path/to/package/vibeface
crypto=/absolute/path/to/package/web/vibeface_secure.js
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-node --skip-codex
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-auto-route --auto-route
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-queue --phase1
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-agent --chat-agent
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-net --network-reliability --net1-lane 7
```

身份、加入和浏览器网关改动跑 `--auto-route`，覆盖建网、目标与 Origin 校验、承诺加短码加入与授予管理权、承诺不符或拒绝作废、撤销、写入签名、CLI 加入、根恢复保留管理设备及未知写入不重放。跨入口转发在第三期随 iroh 恢复。

队列、停止回复、待核对和事件日志改动跑 `--phase1`；工作派发、审批、中断、App Server 托管与崩溃恢复改动跑 `--chat-agent`。两者都是单 Node 场景。统一网络第一期删除了多 Node 同步，不带模式参数的多 Node 全量验收在第三期恢复前不可用。其他网络协议、同步历史与附件传输改动按 `verify-node.py` 中场景选择 `--net1-lane 1` 到 `7`。省略 lane 是 NET-1 基线采集，不代表七个专项通过。lane 1 必须额外提供 `--evidence-dir`，其中包含 `status=observed`、`mode=network-reliability-m0` 的 `m0-baseline.json`，且其源码提交是当前 HEAD 的祖先。部分 lane 会构建插桩程序，应先阅读对应模式要求。

默认模式省略 `--skip-codex` 时还需要本机安装的 Codex，`--codex-bin` 可指定路径；它检查真实会话协议，不发起模型生成。真正模型调用、浏览器渲染、实体设备授权、实际公网与用户后台服务安装需单独按任务验收，不属于 CI 的通过声明。

## 合并门禁

`ci-gate` 要求选中的任务成功，失败、取消、缺失或意外跳过均拒绝通过，并重新计算计划以防弱化覆盖。每次运行都使用固定名称汇总结果。编辑 PR 标题、描述或 base 时也重新检查本次受影响范围，避免全跳过的运行缺少必需门禁。相同 PR 的新运行替代旧运行，核心与扩展检查的分层保持不变。

GitHub 仓库须将 `ci-gate` 设置为必需检查，并要求更新到最新 base 或使用 merge queue，才能强制阻止未通过的合并。扩展检查已移出普通 PR；每日绿色不能替代本次 PR 核心检查，核心绿色也不能当作跨平台或完整进程验收通过。
