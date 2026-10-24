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

Linux 制品核对 checksum、源码提交、版本、target、WASM 和存储 epoch，再以 `verify-node.py --web-only` 启动真实进程，检查网页资源、SPA 路由、CSP、缓存与 404。它不执行多 Node 业务验收，也不验证浏览器渲染。

## 定期、手动和发布检查

每日 UTC 03:23 定时、Actions 中手动运行 `ci`、`v*` tag 强制执行扩展检查，不受路径影响。手动入口只验证和生成制品，不发布。在 GitHub Actions 选择 `ci` → `Run workflow`，选择待验收分支后运行。

| 扩展检查 | 执行范围 |
| --- | --- |
| Rust 快、慢测试及 Clippy | 整个工作区；慢组为精确名称的 `SLOW_TESTS` |
| 最低 Rust 版本 | 安装 Cargo 声明的 MSRV，检查整个工作区 |
| 原生 release 制品 | Linux x64、Linux ARM64、macOS ARM64，分别校验身份并启动网页进程 |
| 单 Node 调度与事件日志验收 | Linux x64 一次，使用 `--phase1` |
| 多 Node 同步、入口与投递验收 | Linux x64 一次，不带模式参数，四台 Node 经 iroh 和成员 relay 互连 |
| 聊天 Agent 模拟协议验收 | macOS ARM64 一次，使用 `--chat-agent` |
| 跨设备协作、换负责人与附件验收 | Linux x64 一次，使用 `--phase4` |
| 公共入口与 relay 令牌验收 | Linux x64 一次，使用 `--public-entry` |
| 写入保护、状态与诊断包验收 | macOS ARM64 一次，使用 `--phase5` |
| Nightly npm 包 | 组装三个原生包，三个平台分别安装、检查版本并执行 `--web-only` 冒烟 |

慢组使用 `opt-level=1`，保留调试断言和整数溢出检查，使用独立缓存。快、慢组运行前校验每个慢测试名恰好匹配一个已有测试。慢组用 `--include-ignored` 运行列出的用例，其他 `ignored` 用例仍默认忽略。

CI 不使用真实 AI Provider 或模型密钥。各模式都使用受控 SDK 和 App Server 夹具，`--phase5` 不启动 App Server。模拟协议通过不能证明真实模型理解需求或正确选择工具。

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
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-node
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-auto-route --auto-route
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-queue --phase1
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-agent --chat-agent
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-phase4 --phase4
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-public-entry --public-entry
python3 scripts/verify-node.py --binary "$binary" --crypto-module "$crypto" --artifacts .artifacts/local-phase5 --phase5
```

身份、加入和浏览器网关改动跑 `--auto-route`，覆盖建网、目标与 Origin 校验、承诺加短码加入与授予管理权、承诺不符或拒绝作废、撤销、写入签名、CLI 加入、经另一台 Node 的 Gateway 跨入口访问、根恢复保留管理设备及未知写入不重放。

Node 之间的传输、同步、地址簿、跨入口转发和投递改动跑默认模式（不带模式参数）。它启动四台真实 Node 和一台其他网络的 Node：P 运行成员 relay，B 只经 relay 连接（模拟严格 NAT），A 负责调度。覆盖经 iroh 加入与同步、签名入口记录（浏览器 WASM 验签，伪造记录被拒）、四条跨入口转发路径与非成员拒绝、成员 Gateway 的 Origin 放行、调度 Node 离线时入口保留并投递且只执行一次、经另一入口重发得到原结果、崩溃与重启后不重放、relay 中断时直连继续而只经 relay 的 Node 在 relay 恢复后追平、入口切换后每条消息只出现一次，以及撤销后不再列出和转发。

队列、停止回复、待核对和事件日志改动跑 `--phase1`；工作派发、App Server 直连、请求处理、分诊、决定回传、中断、托管与崩溃恢复改动跑 `--chat-agent`。两者都是单 Node 场景。`--chat-agent` 的夹具 App Server 启动时核对 `crates/bridge/schema/app-server-methods.json`，缺一个客户端请求即失败；夹具 Agent 用真实 CLI（`vibeface tool dispatch_work`、`vibeface codex call|send|watch|requests|respond`、`vibeface triage events|raise|dismiss`）工作。覆盖直接派发与重试只开一轮、Bot `codex send` 作为工作 steer 进行中的回合（同一 `--id` 只回读、换内容被拒、Bot 轮外拒绝，两项工作共用一个回合并各自回报一次）、未知结果按 clientId 对账不重发、服务端请求由 Agent 应答一次、置之不理时回退拒绝、无 Agent 时立即回退、hook 经分诊在拥有会话的聊天里提问并发出 HMAC 签名 webhook（重试同一 event_id、4xx 进死信、回答后再发 `answered`）、挂住请求的提问经 `answer` 回答一次并由 Bot 的 `answer` 轮应答 Codex 一次、文字回答经 Bot `codex send --id a-<问题>` 作为一项工作只送达一次、外来会话不提问、由运行它的设备归给主 Bot 一次（重复 hook 只写一次，事件照常进分诊；在 Bot 聊天里“转给…”与“不再关注”各写一次、重试不再写，不再关注后静默，换主 Bot 后下一个会话归新主 Bot，之后的提问进接手的 Bot 聊天；`codex exec` 等非交互会话不归属也不分诊），以及 decision token 的 HTTP 回答。

跨设备派发与停止、负责人切换、撤销后放弃、待核对结清、跨入口加入和附件上传改动跑 `--phase4`。它启动四台 Node：A、B、C 都带夹具协调 Agent 和 App Server，P 是运行 relay 的公网角色。跨设备工作是 Agent 交给 Agent：目标设备的 Agent 自己开 Codex 线程；目标上的服务端请求变成拥有该线程的聊天里的提问，在 A 回答后由 A 的 Bot 经 `codex respond --node` 转发应答一次。C 接任务后离线并被撤销。

公共入口、relay 准入和成员 Node 指向入口的改动跑 `--public-entry`。P 是不入网的公共入口，M 只经 P 的 relay 可达，X 持错误令牌；覆盖浏览器经 P 加入 M 并对话、P 重启后重连、错误令牌被拒、decision token 经 P 的 `POST /api/decision` 以 `vibeface/decision/1` 回答 M 上的提问一次、回答经 M 的 Bot 送达线程一次且错误 token 得 401，以及 P 不存事件和附件。

写入闸门、对齐、分叉、日志、`status` 与诊断导出改动跑 `--phase5`。A、B 两台 Node：A 从备份恢复后对齐前拒绝写入、对齐后无分叉；B 离线时 A 只读，用户确认后才写；复制的数据目录在 B 处分叉后只有副本停写并列出冲突，原 A 与 B 继续写；同时检查 `status` 的对端连通与上次同步、`node.log` 权限与内容、诊断包和日志里没有能力、口令或邀请。

真正模型调用、浏览器渲染、实体设备授权、实际公网 NAT 与用户后台服务安装需单独按任务验收，不属于 CI 的通过声明。

## 合并门禁

`ci-gate` 要求选中的任务成功，失败、取消、缺失或意外跳过均拒绝通过，并重新计算计划以防弱化覆盖。每次运行都使用固定名称汇总结果。编辑 PR 标题、描述或 base 时也重新检查本次受影响范围，避免全跳过的运行缺少必需门禁。相同 PR 的新运行替代旧运行，核心与扩展检查的分层保持不变。

GitHub 仓库须将 `ci-gate` 设置为必需检查，并要求更新到最新 base 或使用 merge queue，才能强制阻止未通过的合并。扩展检查已移出普通 PR；每日绿色不能替代本次 PR 核心检查，核心绿色也不能当作跨平台或完整进程验收通过。
