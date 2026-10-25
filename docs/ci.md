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
| Delivery 领域库、proof、合约和服务入口 | Delivery 与 Node 核心检查、新 guest 的公开证明验证和本地 EVM 验收 |
| Cargo manifest、锁文件、工具链、CI 规则、未知输入 | 整个工作区的核心检查 |

静态快检保留 Rust fmt、工作流与 shell 检查、JavaScript 语法、选择器和安装器自检。涉及 Rust 时校验 Atlas 迁移文件，不打开数据库。快速 Rust 测试保留权限、个人网络隔离、数据持久化、写操作去重与未知结果对账等已有回归，只跳过 `SLOW_TESTS` 明确列出的重型用例，目前是 10 万事件追加测试。

改 `crates/node/migrations/` 下的 schema 必须同时提高 `crates/node/src/config.rs` 的 `DATA_EPOCH`，并更新相邻的 `DATA_EPOCH_SCHEMA_SHA256`；否则 Node 快速测试 `schema_changes_bump_the_data_epoch` 失败。存储 epoch 不变时旧数据库能打开，运行中才因缺列出错，自更新也会误判可原地升级。`install.sh` 中写死的 epoch 一并修改，由 `ci.py --self-test` 核对。

Web 路径优先于 Node 路径。Node 对 Protocol、Bridge、Secure 的依赖用于向上扩展检查范围。PR 按 `merge-base(base, head)..head` 的完整差异选取，删除与重命名包括旧、新路径。无法识别的输入选择整个工作区核心检查。

`delivery` 路径先于通用 Node 和 Cargo 路径匹配。它包括 `crates/delivery/`、`proofs/delivery/`、`contracts/delivery/`、`tests/fixtures/delivery-probe/`、`crates/node/src/delivery/`、发现实现、服务入口与本地钱包边界，以及两个 delivery 验收脚本。Delivery 领域库向上触发 Node 检查，因此 Rust 任务所需的 Web 和密码学资源仍会构建。完整工作区检查也要求 `delivery` 任务成功。

Linux 制品核对 checksum、源码提交、版本、target、WASM 和存储 epoch，再以 `verify-node.py --web-only` 启动真实进程，检查网页资源、SPA 路由、CSP、缓存与 404。它不执行多 Node 业务验收，也不验证浏览器渲染。

Linux 制品还使用 [e2e](e2e.md) 启动隔离 Node，在 Chromium 中验证建网页面以及建网、下载恢复文件、跳过 AI 设置、创建 Bot 和刷新后保留的流程。它不调用真实模型。CI 上传浏览器测试报告，排除下载的恢复文件和 Node 数据。

## 可验证交付的核心检查

`delivery` 任务使用 Rust 1.98.1 构建独立的 `proofs/delivery` host。guest 编译器固定为 RISC Zero Rust `r0.1.97.0`，由 `scripts/install-delivery-toolchain.py` 校验官方归档 SHA-256 后安装到 `.artifacts/vdp/risc0/`。它不改全局 Rust 配置，Node 默认构建不依赖重型 proof host。

规范 guest 只在 Linux x86_64 构建。CI 把源码、Cargo 缓存、编译器和产物分别挂到 `/work`、`/cargo`、`/risc0` 和 `/artifacts`，再调用唯一入口 `proofs/delivery/build.sh`。固定路径和 guest 的 remap 参数避免把不同机器的绝对路径写入不同的 image。Mac 原生构建不能作为该规范 image 的证明验收。

`scripts/check-delivery.py` 先要求 `proofs/delivery/fixtures/manifest.json` 和四种关系的公开 Groth16 receipt。它通过规范入口重新编译 guest，核对 image ID、ELF SHA-256、唯一 `Statement.sol` 的 schema hash 和 47 槽 journal，再用本次构建的 host 验证 receipt。验证还要求 host 导出的 Groth16 seal 与对应公开 fixture 逐字一致，拒绝缺失或额外字段。旧 image、过期 schema、错误 journal、缺文件、dev mode 和跳过 guest 构建都会失败。

所有 receipt 还必须绑定产品的 `proofs/delivery/profile.json`。检查要求该文件与重编 guest 的 image、schema 相同，并固定为两个窗口、每窗 65,536 字节、总共 131,072 字节、64,000,000 cycles 和 16 GiB 内存上限。Rust fixture 命令显式读取该文件，以同一 ABI 生成 profile ID，再逐项核对 receipt；默认的 16 窗口测试 profile 不属于产品信任根。guest 不读取产品 profile 文件，避免 image 自引用。

合约检查使用锁定的 solc、Ganache 和 ethers。官方 verifier 源码逐项校验摘要，`compile.mjs --check-trust` 对比重新编译的 runtime 与已提交的 `trust.json`，不改 tracked 文件。检查还包括账务正反例、官方 published fixture、两连续窗口的 Rust 与 Solidity 编码比较，以及当前 delivery image 的真实 seal 在生产 adapter 上的验真和篡改拒绝。

这些结果分别保留在 `.artifacts/vdp/` 和 `.artifacts/vdp/contracts/`。CI 只上传公开 journal、结果和合成公开向量，不上传私密见证、密钥、Node 数据或工具链。官方 published fixture 和账务测试有各自的标签，它们不能替代当前 delivery image 的证明。

在上述固定路径的 Linux sandbox 中执行同一核心检查：

```sh
python3 scripts/check-delivery.py --check-fixtures
python3 scripts/install-delivery-toolchain.py
CARGO_HOME=/cargo RISC0_HOME=/risc0 python3 scripts/check-delivery.py
```

Mac 或未配置上述挂载的 Linux 主机使用同一 sandbox 入口：

```sh
python3 scripts/check-delivery.py --check-fixtures
python3 scripts/check-delivery.py --sandbox
```

`--sandbox` 先检查 fixture，再用固定的 `proofs/delivery/Sandbox.Dockerfile` 构建本地镜像。构建上下文仅含 Dockerfile 和 gnark 启动脚本。容器只挂四个目录，复用 `.artifacts/vdp/cargo`、`.artifacts/vdp/risc0` 和 `.artifacts/vdp/canonical`，不挂 Docker socket，不把 host 凭证或 dev mode 作为参数或挂载传入容器。内存上限为 11 GiB，内存与交换合计也限 11 GiB，最多 64 个进程。容器先在内部安装固定 guest 编译器，再执行默认完整检查。Preflight proof 域使用此入口，Mac 不安装原生 guest 编译器。

`--check-fixtures` 只核对公开文件结构和 schema，不验证证明。两个脚本的 `--self-test` 也只验证工具自身的正反例。完整命令缺少 fixture、编译器、依赖或 verifier 时退出失败，不能把未验证结果记为通过。

CI 不生成新的昂贵证明，也不为新容量或资源上限背书。修改密码关系或 guest 后，先在本地生成并验收新 image 的真实证明，再更新公开 fixture。完整 64 KiB 窗口、连续窗口、实际 prover 和包装进程的资源硬限制、真实 Node 传输与资金流程、断线恢复及原生手机操作仍需按任务单独验收。生产 adapter 的 seal 验真只能证明该公开关系，不能单独证明托管合约已经收付款或用户已读。

当计划选中 `delivery` 时，Linux x64 package 任务还以本次打包的 Node 和 JS/WASM 执行 `verify-node.py --delivery-charity`。它先以 `--locked` 构建独立的 `tests/fixtures/delivery-probe`，产物放在 `.artifacts/delivery-probe/target`。probe 编译时包含当前产品的发现实现，进程验收重新核对该源码摘要。公网角色、不同个人网络的 Node、真实 loopback QUIC/Noise、免费发现、公益文件恢复及私人协议拒绝使用真实进程验证。CI 仅上传该模式的公开 `report.json`。

公益进程验收与 paid proof 任务各自必须通过。公益收据不代表链上交付或结算。Preflight 对 probe 和相关服务改动单独运行 fmt 与最小 locked build，不把这一构建当作进程验收。

## 手动生成真实付费交付证据

现有 `ci` 的手动入口提供 `delivery_live` 布尔选项，默认关闭。只有 `workflow_dispatch` 显式选中它时才运行 `delivery-live`，并额外要求该任务成功。普通 PR、push、定时检查和节点默认付费开关沿用原规则。

该任务读取同一 run 的 Linux x64 Node 包、Web 和 WASM，核对归档摘要、源码提交及生成资源的隐私收据。固定 sandbox 先运行打包 Node 的真实网页进程。只有明确的 GLIBC symbol 或 loader 不兼容才从同一提交和已经验证的 Web/WASM 实际构建 Node，并单独记录这一来源。业务或验收失败直接阻塞任务。

任务随后安装固定 guest 编译器，重编实际 guest 和 host，核对产品 profile，构建真实 QUIC probe，再写入权限为 `0600` 的全新本机部署配置。它不依赖已有证明 fixture，而是调用 `verify-node.py --delivery` 串行产生七份真实证明，检查原文件、实际 29 TestToken 提现及独立只读 observer。测试链限 loopback 31337，不使用真实资产、公共部署或 AI Provider。

整个 Node、EVM 和 prover 流程在既定四挂载、11 GiB 内存、零交换及 64 进程的同一 cgroup 中运行，Node 使用两个 Tokio worker threads。公开资源记录来自实际 kernel cgroup 文件。上传清单仅含七份公开 receipt 与对应 JSON、含 capsule 和交易哈希的 settlement bundle、只读观察结果和安全 summary/resource；Node 数据、钱包、私密见证和原始日志留在临时工作区。任务失败、缺少证明或观察未通过均不能标成成功。

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

无需编译项目即可验证 CI 规则和发布隐私检查：

```sh
python3 scripts/ci.py --self-test
python3 scripts/ci.py plan --base origin/main --head HEAD --event pull_request
actionlint .github/workflows/ci.yml
```

自检会编译一个独立 Rust 夹具并打包，核对调用方编译参数仍有效、源码路径已替换、归档不含构建账号或扩展属性。

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

身份、加入和浏览器网关改动跑 `--auto-route`，覆盖建网、目标与 Origin 校验、承诺加短码加入与授予管理权、承诺不符或拒绝作废、撤销、写入签名、CLI 加入、经另一台 Node 的 Gateway 跨入口访问、根恢复进入 48 小时等待、被现有管理设备拦下后旧恢复文件仍有效、空 Node 上的找回同样等待，以及未知写入不重放。

Node 之间的传输、同步、地址簿、跨入口转发和投递改动跑默认模式（不带模式参数）。它启动四台真实 Node 和一台其他网络的 Node：P 运行成员 relay，B 只经 relay 连接（模拟严格 NAT），A 负责调度。覆盖经 iroh 加入与同步、签名入口记录（浏览器 WASM 验签，伪造记录被拒）、四条跨入口转发路径与非成员拒绝、成员 Gateway 的 Origin 放行、调度 Node 离线时入口保留并投递且只执行一次、经另一入口重发得到原结果、崩溃与重启后不重放、relay 中断时直连继续而只经 relay 的 Node 在 relay 恢复后追平、入口切换后每条消息只出现一次，以及撤销后不再列出和转发。

队列、停止回复、待核对和事件日志改动跑 `--phase1`；工作派发、App Server 直连、请求处理、分诊、决定回传、中断、托管与崩溃恢复改动跑 `--chat-agent`。两者都是单 Node 场景。`--chat-agent` 的夹具 App Server 启动时核对 `crates/bridge/schema/app-server-methods.json`，缺一个客户端请求即失败；夹具 Agent 用真实 CLI（`vibeface tool dispatch_work`、`vibeface codex call|send|watch|requests|respond`、`vibeface triage events|raise|dismiss`）工作。覆盖直接派发与重试只开一轮、Bot `codex send` 作为工作 steer 进行中的回合（同一 `--id` 只回读、换内容被拒、Bot 轮外拒绝，两项工作共用一个回合并各自回报一次）、未知结果按 clientId 对账不重发、服务端请求由 Agent 应答一次、置之不理时回退拒绝、无 Agent 时立即回退、hook 经分诊在拥有会话的聊天里提问并发出 HMAC 签名 webhook（重试同一 event_id、4xx 进死信、回答后再发 `answered`）、挂住请求的提问经 `answer` 回答一次并由 Bot 的 `answer` 轮应答 Codex 一次、文字回答经 Bot `codex send --id a-<问题>` 作为一项工作只送达一次、外来会话不提问、由运行它的设备归给主 Bot 一次（重复 hook 只写一次，事件照常进分诊；Bot 轮次列出名下会话，Bot 用 `hand_over_session` 转给其他 Bot 与不再关注各写一次、重试只读回、过期版本被拒、转走后原 Bot 不能再改，不再关注后静默，换主 Bot 后下一个会话归新主 Bot，之后的提问进接手的 Bot 聊天；`codex exec` 等非交互会话不归属也不分诊），以及 decision token 经 HTTP 回答拥有会话的提问一次、回答经 Bot 作为一项工作送达会话一次（不向已失效的会话发送）。

跨设备派发与停止、负责人切换、撤销后放弃、待核对结清、跨入口加入和附件上传改动跑 `--phase4`。它启动四台 Node：A、B、C 都带夹具协调 Agent 和 App Server，P 是运行 relay 的公网角色。跨设备工作是 Agent 交给 Agent：目标设备的 Agent 自己开 Codex 线程；目标上的服务端请求变成拥有该线程的聊天里的提问，在 A 回答后由 A 的 Bot 经 `codex respond --node` 转发应答一次。C 接任务后离线并被撤销。

公共入口、relay 准入和成员 Node 指向入口的改动跑 `--public-entry`。P 是不入网的公共入口，M 只经 P 的 relay 可达，X 持错误令牌；覆盖浏览器经 P 加入 M 并对话、P 重启后重连、错误令牌被拒、decision token 经 P 的 `POST /api/decision` 以 `vibeface/decision/1` 回答 M 上的提问一次、回答经 M 的 Bot 送达线程一次且错误 token 得 401，以及 P 不存事件和附件。

写入闸门、对齐、分叉、日志、`status` 与诊断导出改动跑 `--phase5`。A、B 两台 Node：A 从备份恢复后对齐前拒绝写入、对齐后无分叉；B 离线时 A 只读，用户确认后才写；复制的数据目录在 B 处分叉后只有副本停写并列出冲突，原 A 与 B 继续写；同时检查 `status` 的对端连通与上次同步、`node.log` 权限与内容、诊断包和日志里没有能力、口令或邀请。

真正模型调用、浏览器渲染、实体设备授权、实际公网 NAT 与用户后台服务安装需单独按任务验收，不属于 CI 的通过声明。

## 合并门禁

`ci-gate` 要求选中的任务成功，失败、取消、缺失或意外跳过均拒绝通过，并重新计算计划以防弱化覆盖。每次运行都使用固定名称汇总结果。编辑 PR 标题、描述或 base 时也重新检查本次受影响范围，避免全跳过的运行缺少必需门禁。相同 PR 的新运行替代旧运行，核心与扩展检查的分层保持不变。

GitHub 仓库须将 `ci-gate` 设置为必需检查，并要求更新到最新 base 或使用 merge queue，才能强制阻止未通过的合并。扩展检查已移出普通 PR；每日绿色不能替代本次 PR 核心检查，核心绿色也不能当作跨平台或完整进程验收通过。
