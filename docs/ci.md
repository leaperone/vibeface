---
title: 按改动选择 CI
description: 查看 Vibeface CI 如何按改动范围选择检查。
---

# 按改动选择 CI

`.github/workflows/ci.yml` 使用 `scripts/ci.py` 选择检查范围、Rust 测试分组和必需任务。每次触发都生成计划，`ci-gate` 要求选中的任务全部成功。没有顶层 `paths` 筛选。

开发分支的 PR 不触发 GitHub Actions，也不要求 CI 状态。需要验证移动端或 macOS 时，从 Actions 手动运行对应 workflow。`main` push、定时、手动运行、merge queue 和 `v*` Tag 仍按各自计划检查；推送 `v*` Tag 会运行完整发布检查、原生制品和应用发布构建，合并后的制品与实际体验由开发者自行验收。

## main 与 merge queue 的核心检查

开发分支 PR 不运行本节检查。`main` push 按路径运行核心检查；merge queue 无路径筛选，执行整个工作区的核心检查。

| 改动 | 核心检查 |
| --- | --- |
| README、项目指引、SDK README | 静态快检，不构建或运行 Rust 测试 |
| `apps/www/`、`docs/` | 门户类型检查、静态构建和产物检查，不运行 Rust 测试 |
| `apps/app/`、Web 构建脚本 | 浏览器密码学互通、Web 测试与类型检查、静态构建、Linux x64 完整包及网页进程验收 |
| 共享主题、根 JS package、pnpm workspace 或 lockfile | Node Web 检查与门户检查 |
| Node Rust、迁移、SDK、安装、打包或进程验收脚本 | Node 快速测试、Clippy、Web 和 Linux x64 制品启动验收 |
| Bridge | Bridge 和 Node 的核心检查 |
| Protocol | Protocol、Bridge 和 Node 的核心检查 |
| Secure、密码学验收脚本 | Secure、Node 和浏览器密码学互通 |
| Delivery 领域库、proof、合约和服务入口 | Delivery 与 Node 核心检查、新 guest 的公开证明验证和本地 EVM 验收 |
| Cargo manifest、`Cargo.lock`、工具链、CI 规则、未知输入 | 整个工作区的核心检查 |

静态快检保留 Rust fmt、工作流与 shell 检查、JavaScript 语法、选择器和安装器自检。涉及 Rust 时校验 Atlas 迁移文件，不打开数据库。快速 Rust 测试保留权限、个人网络隔离、数据持久化、写操作去重与未知结果对账等已有回归，只跳过 `SLOW_TESTS` 明确列出的重型用例，目前是 10 万事件追加测试和超出待补依赖上限的 4 万事件追平测试。

`www` 使用 Node.js 24 和冻结的 pnpm 依赖，依次运行 `typecheck`、`build`、`check:static`，上传 `apps/www/dist/client` 为 `www-<SHA>` 制品。门户与独立 App 分别构建；根 `docs/` 是门户文档正文的唯一来源，修改文档也会触发 `www`。

改 `apps/core/migrations/` 下的 schema 必须同时提高共享控制 crate `crates/control/src/lib.rs` 的 `DATA_EPOCH`，并更新 `apps/core/src/config.rs` 中相邻的 `DATA_EPOCH_SCHEMA_SHA256`；否则 Node 快速测试 `schema_changes_bump_the_data_epoch` 失败。存储 epoch 不变时旧数据库能打开，运行中才因缺列出错，自更新也会误判可原地升级。`install.sh` 中写死的 epoch 一并修改，由 `ci.py --self-test` 核对。

Web 路径优先于 Node 路径，只有 `packages/client/src/avatar/face.ts` 与 `derived-faces.json` 例外：它们按 Node 路径选取，因为这组向量也是 Node Rust 测试的输入，核对 Node 由 bot ID 派生的长相与客户端一致；Node 受影响时 Web 检查照常运行。Node 对 Protocol、Bridge、Secure 的依赖用于向上扩展检查范围。离线模拟 PR 计划时按 `merge-base(base, head)..head` 的完整差异选取，删除与重命名包括旧、新路径。无法识别的输入选择整个工作区核心检查。

`delivery` 路径先于通用 Node 和 Cargo 路径匹配。它包括 `crates/delivery/`、`proofs/delivery/`、`contracts/delivery/`、`tests/fixtures/delivery-probe/`、`apps/core/src/delivery/`、发现实现、服务入口与本地钱包边界，以及两个 delivery 验收脚本。Delivery 领域库向上触发 Node 检查，因此 Rust 任务所需的 Web 和密码学资源仍会构建。完整工作区检查也要求 `delivery` 任务成功。

Linux 制品核对 checksum、源码提交、版本、target、WASM 和存储 epoch，再以 `verify-node.py --web-only` 启动真实进程，检查网页资源、SPA 路由、CSP、缓存与 404。它不执行多 Node 业务验收，也不验证浏览器渲染。

Linux 制品还使用 [e2e](e2e.md) 启动隔离 Node，在 Chromium 中验证建网页面以及建网、下载恢复文件、跳过 AI 设置、创建 Bot 和刷新后保留的流程。它不调用真实模型。CI 上传浏览器测试报告，排除下载的恢复文件和 Node 数据。

## 可验证交付的核心检查

`delivery` 任务使用 Rust 1.98.1 构建独立的 `proofs/delivery` host。guest 编译器固定为 RISC Zero Rust `r0.1.97.0`，由 `scripts/install-delivery-toolchain.py` 校验官方归档 SHA-256 后安装到 `.artifacts/vdp/risc0/`。它不改全局 Rust 配置，Node 默认构建不依赖重型 proof host。

规范 guest 只在 Linux x86_64 构建。CI 把源码、Cargo 缓存、编译器和产物分别挂到 `/work`、`/cargo`、`/risc0` 和 `/artifacts`，再调用唯一入口 `proofs/delivery/build.sh`。固定路径和 guest 的 remap 参数避免把不同机器的绝对路径写入不同的 image。Mac 原生构建不能作为该规范 image 的证明验收。

`scripts/check-delivery.py` 读取 `proofs/delivery/fixtures/manifest.json` 和四种关系的公开 Groth16 receipt。它通过规范入口重新编译 guest，核对 image ID、ELF SHA-256、唯一 `Statement.sol` 的 schema hash 和 47 槽 journal，再用本次构建的 host 验证 receipt。验证还要求 host 导出的 Groth16 seal 与对应公开 fixture 逐字一致，拒绝缺失或额外字段。旧 image、过期 schema、错误 journal、缺文件、dev mode 和跳过 guest 构建都会失败。

现状：这 13 个证明夹具需要在 x86_64 大内存机器上生成，CI 的 11 GiB runner 和 ARM Mac 都生成不了，仓库暂不包含 `proofs/delivery/fixtures/`。目录不存在时，检查打印 `skipped: no public proof fixture`，跳过 receipt 验证和 `verify-delivery.mjs`，guest 重编、profile、跨语言向量和其余合约检查照常运行。目录一旦提交就恢复完整验证，目录存在但内容不全仍然失败。

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

`--check-fixtures` 只核对公开文件结构和 schema，不验证证明。两个脚本的 `--self-test` 也只验证工具自身的正反例。整个 fixture 目录不存在时按上文跳过 receipt 验证；fixture 不全、缺编译器、依赖或 verifier 时退出失败，不能把未验证结果记为通过。

开发分支 PR 不生成新的昂贵证明，也不为新容量或资源上限背书。显式手动选中的 `delivery_live` 按下文生成新的真实证明。修改密码关系或 guest 后，先验收新 image 的真实证明，再更新公开 fixture。生产 adapter 的 seal 验真只能证明该公开关系，不能单独证明托管合约已经收付款或用户已读。

当计划选中 `delivery` 时，Linux x64 package 任务还以本次打包的 Node 和 JS/WASM 执行 `verify-node.py --delivery-charity`。它先以 `--locked` 构建独立的 `tests/fixtures/delivery-probe`，产物放在 `.artifacts/delivery-probe/target`。probe 编译时包含当前产品的发现实现，进程验收重新核对该源码摘要。公网角色、不同个人网络的 Node、真实 loopback QUIC/Noise、免费发现、公益文件恢复及私人协议拒绝使用真实进程验证。CI 仅上传该模式的公开 `report.json`。

公益进程验收与 paid proof 任务各自必须通过。公益收据不代表链上交付或结算。Preflight 对 probe 和相关服务改动单独运行 fmt 与最小 locked build，不把这一构建当作进程验收。

## 手动生成真实付费交付证据

现有 `ci` 的手动入口提供 `delivery_live` 布尔选项，默认关闭。只有 `workflow_dispatch` 显式选中它时才运行 `delivery-live`，并额外要求该任务成功。开发分支 PR 不触发此入口；push、定时检查和节点默认付费开关沿用原规则。

该入口还通过 `mobile.yml` 的 `workflow_call` 构建同一提交的 Android release APK，不构建 iOS。Android 构建记录实际 APK 摘要、release variant、application id，以及 arm64-v8a 和 x86_64 两份安全库的 ELF ABI 与摘要。`delivery-live` 只接收同一 run、同一 SHA 的 Android、Linux x64 Node 包、Web 和 WASM，并重新核对源码、归档和原生库字节及生成资源的隐私收据。

Android 的 UBRN 配置保留项目自行维护的 `android/proguard-rules.pro`，与现有 CMake、Gradle 和 manifest 一样列入 `noOverwrite`。生成器不得以通用 JNA 规则覆盖实际 React Native 模块的 R8 keep 规则。打包后仍要求整个源码 checkout 与本次提交一致，不通过恢复文件或忽略 dirty 状态补造来源。

固定 sandbox 先运行打包 Node 的真实网页进程。只有明确的 GLIBC symbol 或 loader 不兼容才从同一提交和已经验证的 Web/WASM 实际构建 Node，并单独记录这一来源。业务或验收失败直接阻塞任务。任务安装固定 guest 编译器，重编实际 guest 和 host，核对产品 profile，构建真实 QUIC probe，再写入权限为 `0600` 的全新本机部署配置。

`verify-node.py --delivery` 生成十三份唯一真实证明。严格最大文件订单产生七份，原文件必须覆盖实际 130816 字节边界，独立只读 observer 验证实际 29 TestToken 提现。rotate、revoke、register 产生三份新的密钥证明，记录实际 serial、intent、交易及 journal 和 receipt 摘要。普通订单复用最后有效的 register 证明，由真实网页选文件、确认报价和付款，真实 Android 接收、断线、恢复并保存 4096 字节原文件，再产生三份 source、hold、open 证明。普通订单使用自己的四份公开证明 bundle 和只读 observer，独立核对 17 TestToken 提现。复用的 binding 不计为新证明。

同一严格订单还执行 16 项客户端权限检查、7 项有效 Agent token 检查、8 项服务授权检查，以及三台 Node 各自的 RPC、SYNC、BLOB 私人协议拒绝。真实普通成员和本机会话保留同网络读权限，写入和付款需要管理成员。Agent 工具正例在拒绝前后都成功，过期服务授权使用实际 Provider 正确签名。检查时钱包实际解锁，财务、对象、设置、DAG 和工作记录保持不变，正常 Agent 完成只增加原回合的一条回复。公开 summary 只包含固定计数和前后状态摘要，不导出 token、服务授权签名字节或本机启动材料。

外层脚本在宿主启动 Chromium 和专属 root AOSP x86_64 AVD。受认证的 loopback controller 发布私有 `0600` 描述文件后，两个 e2e worker 同时驱动 Web 和指定 Android APK。Node 验收与 e2e 必须同时成功，订单、intent、文件 SHA-256、离线观察及真实保存记录均须一致。仅退出码为零、跳过测试或已有公开 fixture 均不能通过。任一进程失败会终止本任务另一进程，恢复本任务接收端的 ADB reverse，并关闭专属 AVD 和私有 ADB server。

controller 在真实 key1 后、C0 快照前开放配对，成员随原节点数据进入全部分支。`stage` 明确区分 `bootstrap`、`inspections`、`ordinary`、`complete`。两个 UI 提前配对并观察原订单的部分退款、真实 HTTP502 未知交易，以及接收密钥 key1 到 Rotate2。检查时钱包锁定，广播数为零，proof 和金融 intent 行保持不变，故障清除后才确认检查。只有 `stage=ordinary` 才允许普通订单的 prepare、confirm、accept。单次 inspection 有独立上限；整个 UI 测试与宿主 join 使用既有 LIVE 剩余总截止，普通订单自身仍保留原一小时上限。

`ui-inspections.json` 只包含四次真实 Node 和 PNG 交叉验证的检查结果，生成证明数为零。外层按严格订单的 public journal 绑定 source/receiver Peer 和 key ID、serial，再核对实际分支的 12/12/17、未知状态下撤回的窗口证明，以及四张 PNG 的字节摘要。普通订单另保留 `web-settled.png`，与原 17/17/0 及真实 Web 验收共验。真实 provider 已提交前缀、held 和 key-published 的 SIGKILL 恢复结果另按固定 case 投影，明确复用了原订单的缓存材料。

整个 Node、EVM、prover 和其包装进程保持 root、固定 HOME、既定四挂载及同一 cgroup，内存上限为 11 GiB，零交换，上限为 64 个进程。Node 使用两个 Tokio worker threads。容器只增加 host network，共享本机 loopback。宿主 UI 和 AVD 不计入该 proof cgroup 的资源记录。任何 OOM 计数增长、资源观察失败或缺少证明均失败。只有 proof、两个真实 UI 和清理全部成功后，宿主才把 `PROOFS_PASS` 更新为最终 `PASS`。

若 paid process 后续失败，外层只尝试保留本次唯一 summary 对应的 strict 七份公开证明。全部 receipt 必须由当前 host 再次验真，绑定当前 image、profile、journal 与 receipt 摘要，且原独立 observer 确认 strict 29 的实际收付款，才整组发布。缺少任何数据均不发布该组。`partialEvidence.scope=strict-only` 始终伴随整体 FAIL，不代表分支恢复、普通 UI 或当前 canonical 状态通过。UI 先失败时，固定 controller abort 后仅在原总截止内给收尾留证一个有界窗口，随后执行原定清理并保留原异常。

完整验收还要求本次 session 的固定 branch index、全部 24 个有限分支、三个 key 观察及 core-restored。逐项核对 index 的文件 SHA-256、真实 ledger、Node 观察范围、同一生命周期 journal 和恢复后的 strict 29 TestToken 结果。缺少任一分支、恢复仍为 pending、旧文件摘要不同或只剩证明总数，均不能通过。incoming 分支只证明原 Node 签名交易在受控原链的实际资金行为，保留 `NOT_TESTED_ORIGINAL_BROADCAST_RAW_ONLY` 标记。

宿主用 `xvfb-run` 分配专用 DISPLAY、`1440x1000x24` 屏幕及任务内的 `0600` XAUTHORITY，Web 使用 headed 模式。e2e 全程视频仍关闭。测试只在配对后录制三个指定短片，Web 为报价确认和同 intent 重载，Android 为接收后离线与恢复后保存。每段最多 180 秒。宿主独立核对 MP4 容器、视频流、时长、文件 SHA-256 和真实退出方式，合并两端独立收据为 `video-metadata.json`。ffmpeg 必须以 q 正常退出，Android screenrecord 保留实际 SIGINT 对应的退出码或信号。

上传清单仅含公开 receipt 与对应 JSON、两个 settlement bundle 及各自 observer、安全 summary/resource、固定白名单中的分支与恢复记录、原有五个 UI JSON 和 PNG、检查清单及指定新增五张 PNG，以及 `web-confirm.mp4`、`android-accept-offline.mp4`、`android-resume-save.mp4` 和对应合并元数据。分支记录是受控分支的历史证据，不能当作恢复后的当前 canonical 状态。join 邀请、全程 attempt 视频、trace、controller token、原始文件、Node 数据、钱包、私密见证及原始日志不上传。测试链限 loopback 31337，不使用真实资产、公共部署或 AI Provider，也不据此宣称绝对链终局性或 iOS 付费验收。

## 定期、手动和发布检查

每日 UTC 03:23 定时、Actions 中手动运行 `ci` 和 `v*` tag 强制执行扩展检查，不受路径影响。merge queue 执行整个工作区的核心检查，不启用慢测试、MSRV 或三平台打包。开发分支 PR 不触发 `ci`。`v*` tag 还会构建 macOS 和 Android 发布资产；手动入口只验证和生成制品，不发布。在 GitHub Actions 选择 `ci` → `Run workflow`，选择待验收分支后运行。

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
| 仅发现、普通 relay 回退与无网页验收 | Linux x64 一次，使用 `--discovery` |
| 写入保护、状态与诊断包验收 | macOS ARM64 一次，使用 `--phase5` |
| Nightly npm 包 | 组装三个原生包，三个平台分别安装、检查版本并执行 `--web-only` 冒烟 |

快、慢测试组均使用 `opt-level=1`，保留调试断言和整数溢出检查，调试信息使用 `line-tables-only` 保留回溯文件和行号，共享独立的 `test-opt1` Rust 缓存；Clippy 使用单独的 `check` 缓存。两组运行前校验每个慢测试名恰好匹配一个已有测试。慢组用 `--include-ignored` 运行列出的用例，其他 `ignored` 用例仍默认忽略。两组测试命令用 `/usr/bin/time -v` 记录 CPU 与 maxRSS；maxRSS 不能当作并发进程树的总峰值。

CI 不使用真实 AI Provider 或模型密钥。各模式都使用受控 SDK 和 App Server 夹具，`--phase5` 不启动 App Server。模拟协议通过不能证明真实模型理解需求或正确选择工具。

只有版本 tag 可以发布。tag 必须与 Cargo 版本一致且提交已进入 `main`；扩展 gate、三平台制品及适用的 npm 检查必须通过。发布前继续校验资产清单并下载逐字节比对，详见 [构建与发布](releasing.md)。

## 本地按改动补充验收

开发者修改同步、恢复、调度或工具执行时，必须执行对应专项并记录命令及结果；不能只依赖下次定期运行。每次只跑相关模式。安装器或平台交付改动应额外手动触发 `ci`，确认目标平台制品。

无需编译项目即可验证 CI 规则和发布隐私检查：

```sh
python3 scripts/ci.py --self-test
python3 scripts/ci.py plan --base origin/main --head HEAD --event pull_request
actionlint .github/workflows/ci.yml
```

自检会编译一个独立 Rust 夹具并打包，核对调用方编译参数仍有效、源码路径已替换、归档不含构建账号或扩展属性。

Rust 快、慢组可独立运行。Clippy 默认不安装 MSRV，需要时显式提供：

```sh
python3 scripts/ci.py test --tier fast --packages vibeface-bridge vibeface-core vibeface-control vibeface
python3 scripts/ci.py test --tier slow --packages vibeface-core
python3 scripts/ci.py check --packages vibeface-core vibeface-control vibeface
python3 scripts/ci.py check --packages vibeface-core vibeface-control vibeface --msrv 1.91
```

MSRV 参数应使用当前 `Cargo.toml` 的 `rust-version`。进程专项需要 Python 3、Node.js、当前提交构建的 CLI/Core 程序和真实 JS/WASM 密码学资源。先按 [源码构建步骤](releasing.md#ci-验证范围) 生成资源和制品，然后设置以下路径：

```sh
binary=/absolute/path/to/package/vibeface
core=/absolute/path/to/package/vibeface-core
crypto=/absolute/path/to/package/web/vibeface_secure.js
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-node
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-auto-route --auto-route
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-queue --phase1
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-agent --chat-agent
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-memory --memory-sync
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-phase4 --phase4
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-public-entry --public-entry
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-discovery --discovery
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-phase5 --phase5
```

Memory 签名、版本、归约或用户操作改动补跑 `--memory-sync`。它冻结 binary、JS 和 WASM，启动两台隔离 Node，用真实 WASM 用户签名和受控 Agent callback 验证用户编辑、删除墓碑、Agent 并发分支、跨入口旧签名重封装、完整版本与来源、共享冲突和重启后的签名日志重导入。该模式不调用真实模型，也不替代 Web/Mobile 渲染、默认同步专项或 `--phase1`。各进程专项串行运行，避免固定 UDP 端口竞争。

身份、加入和浏览器网关改动跑 `--auto-route`，覆盖建网、目标与 Origin 校验、承诺加短码加入与授予管理权、承诺不符或拒绝作废、撤销、写入签名、CLI 加入、经另一台 Node 的 Gateway 跨入口访问、根恢复进入 48 小时等待、现有管理设备签名确认后立即生效、被现有管理设备拦下后旧恢复文件仍有效、空 Node 上的找回同样等待，以及未知写入不重放。

Node 之间的传输、同步、地址簿、跨入口转发和投递改动跑默认模式（不带模式参数）。它启动四台真实 Node 和一台其他网络的 Node：P 运行成员 relay，B 只绑定 IPv6 loopback，其他 Node 只绑定 IPv4 loopback，协议族隔离使 B 必须经 relay 连接，A 负责调度。覆盖经 iroh 加入与同步、签名入口记录（浏览器 WASM 验签，伪造记录被拒）、四条跨入口转发路径与非成员拒绝、成员 Gateway 的 Origin 放行、调度 Node 离线时入口保留并投递且只执行一次、经另一入口重发得到原结果、崩溃与重启后不重放、relay 中断时直连继续而只经 relay 的 Node 在 relay 恢复后追平、入口切换后每条消息只出现一次，以及撤销后不再列出和转发。

队列、停止回复、待核对和事件日志改动跑 `--phase1`；工作派发、App Server 直连、请求处理、分诊、决定回传、中断、托管与崩溃恢复改动跑 `--chat-agent`。两者都是单 Node 场景。`--chat-agent` 的夹具 App Server 启动时核对 `crates/bridge/schema/app-server-methods.json`，缺一个客户端请求即失败；夹具 Agent 用真实 CLI（`vibeface tool dispatch_work`、`vibeface codex call|send|watch`、`vibeface requests|respond`、`vibeface triage events|raise|dismiss`）工作。覆盖直接派发与重试只开一轮、Bot `codex send` 作为工作 steer 进行中的回合（同一 `--id` 只回读、换内容被拒、Bot 轮外拒绝，两项工作共用一个回合并各自回报一次）、未知结果按 clientId 对账不重发、服务端请求由 Agent 应答一次、置之不理时回退拒绝、无 Agent 时立即回退、hook 经分诊在拥有会话的聊天里提问并发出 HMAC 签名 webhook（重试同一 event_id、4xx 进死信、回答后再发 `answered`）、挂住请求的提问经 `answer` 回答一次并由 Bot 的 `answer` 轮应答 Codex 一次、文字回答经 Bot `codex send --id a-<问题>` 作为一项工作只送达一次、外来会话不提问、由运行它的设备归给主 Bot 一次（重复 hook 只写一次，事件照常进分诊；Bot 轮次列出名下会话，Bot 用 `hand_over_session` 转给其他 Bot 与不再关注各写一次、重试只读回、过期版本被拒、转走后原 Bot 不能再改，不再关注后静默，换主 Bot 后下一个会话归新主 Bot，之后的提问进接手的 Bot 聊天；`codex exec` 等非交互会话不归属也不分诊），以及 decision token 经 HTTP 回答拥有会话的提问一次、回答经 Bot 作为一项工作送达会话一次（不向已失效的会话发送）。

跨设备派发与停止、负责人切换、撤销后结果待确认、待核对结清、跨入口加入和附件上传改动跑 `--phase4`。它启动四台 Node：A、B、C 都带夹具协调 Agent 和 App Server，P 是运行 relay 的公网角色。跨设备工作是 Agent 交给 Agent：目标设备的 Agent 自己开 Codex 线程；目标上的服务端请求变成拥有该线程的聊天里的提问，在 A 回答后由 A 的 Bot 经 `respond --node` 转发应答一次。C 接任务后离线并被撤销。

公共入口、relay 准入和成员 Node 指向入口的改动跑 `--public-entry`。P 是不入网的公共入口，M 只经 P 的 relay 可达，X 持错误令牌；覆盖浏览器经 P 加入 M 并对话、P 重启后重连、错误令牌被拒、decision token 经 P 的 `POST /api/decision` 以 `vibeface/decision/1` 回答 M 上的提问一次、回答经 M 的 Bot 送达线程一次且错误 token 得 401，以及 P 不存事件和附件。

发现协议、连接协助或 Web 开关改动另跑 `--discovery`。它覆盖没有地址提示的 CLI 邀请加入、清空地址缓存后经重启的发现服务直连、协议族不兼容时发现服务不转发业务、配置普通成员 relay 后自动回退，以及无网页模式下 hook、工具鉴权、维护和同步仍可用。真实公网 NAT 与真实 Agent 工具执行另行验收。

写入闸门、对齐、分叉、日志、`status` 与诊断导出改动跑 `--phase5`。A、B 两台 Node：A 从备份恢复后对齐前拒绝写入、对齐后无分叉；B 离线时 A 只读，用户确认后才写；复制的数据目录在 B 处分叉后只有副本停写并列出冲突，原 A 与 B 继续写；同时检查 `status` 的对端连通与上次同步、`node.log` 权限与内容、诊断包和日志里没有能力、口令或邀请。

CI 固定的 Chromium 用例覆盖指定 Web 流程；任务特定的页面验收、其他浏览器渲染、真正模型调用、实体设备授权、实际公网 NAT 与用户后台服务安装需单独按任务验收，不属于 CI 的通过声明。

## 缓存与并发

`ci` 工作流按事件类型和 Git ref 分组。开发分支 PR 不触发该工作流；主分支 push、定时、手动、merge queue 和 tag 不取消正在执行的同组运行。主分支核心检查可以与定时完整检查同时进行。各组仍遵循 GitHub 最多保留一个等待运行的规则。

Linux x64 制品任务使用 pnpm 缓存，并按 runner、架构和 `pnpm-lock.yaml` 缓存 Chromium 二进制。浏览器的系统依赖从 Ubuntu 官方镜像下载，沿用签名仓库配置。每次仍执行冻结依赖安装、浏览器与系统依赖安装以及完整浏览器用例。缓存只包含浏览器文件，不包含测试结果、Node 数据或下载的恢复文件。

`ci` 工作流中的 Rust 缓存仅在 `refs/heads/main` 或显式 `workflow_dispatch` 时保存；自动 tag push 只读取缓存。手动运行可为相同 ref 的后续运行预热。新增 profile 或缓存 key 在首次保存前需要冷构建。

Android 使用官方 Gradle 缓存动作和 Gradle build cache，保留 `arm64-v8a` 与 `x86_64` 的 release 构建。手动运行可以写入其 Git ref 的缓存。缓存命中仍需执行原构建和制品检查，不能替代本次提交的验收。

## 合并门禁

`ci-gate` 仍要求选中的任务成功，失败、取消、缺失或意外跳过均拒绝通过，并重新计算计划以防弱化覆盖。每次运行都使用固定名称汇总结果。它服务于 main、merge queue、定时、手动和 Tag 运行，不是开发分支 PR 的必需检查。

GitHub 主分支规则不要求 PR 先更新到最新 base，也不将 `ci-gate` 列为必需状态。开发分支可以直接合并；只有版本 Tag 的完整 gate、制品和发布检查仍是发布路径的一部分。
