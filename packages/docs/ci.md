---
title: 按改动选择 CI
description: 查看 Vibeface CI 如何按改动范围选择检查。
---

# 按改动选择 CI

`.github/workflows/ci.yml` 使用 `scripts/ci.py` 选择检查范围、Rust 测试分组和必需任务。常规 CI 运行生成计划，`ci-gate` 要求选中的任务全部成功。`delivery_small` 的独立实验流程见下文。没有顶层 `paths` 筛选。

开发分支的 PR 不触发 GitHub Actions，也不要求 CI 状态。需要验证移动端或 macOS 时，从 Actions 手动运行对应 workflow。普通 `ci` 由 `main` push、定时、手动运行和 merge queue 触发。产品 tag 交给 `release.yml` 构建完整产品，官网 tag 独立构建官网。合并、CI 通过、发布和部署各有自己的验收结果。

| 反馈层 | 入口 | 结果含义 |
| --- | --- | --- |
| 编辑反馈 | Web 的 Vite HMR、按需 Rust 增量构建 | 辅助开发，不证明重启、恢复或制品可用 |
| 本地相关检查 | `ci.py plan`、选中包的检查与进程专项 | 核对本次改动涉及的行为 |
| PR 验收 | 独立工作树和同一个 PR，创建后及最终 HEAD 执行 `preflight --no-merge` | 检查固定 base、HEAD、范围及必要证据，不自动合并 |
| 合并后检查 | 经明确授权合并，再核对 `main` 的 `ci-gate` | 证明该运行选中的检查成功，不发布应用 |
| 制品发布 | 显式应用 tag 与 `release.yml` | 校验产品制品和发布记录，不更新线上服务 |
| 服务部署 | 独立 deploy 命令与对应部署 workflow | 消费已发布版本，核对实际运行版本和服务行为 |

## main 与 merge queue 的核心检查

开发分支 PR 不运行本节检查。`main` push 按路径运行核心检查；merge queue 无路径筛选，执行整个工作区的核心检查。

自动 `main` push 从同仓库 `ci.yml` 最近的成功 main push 运行中选择已验证的祖先提交，以该提交到当前 HEAD 的累计差异选取检查。较早的 Rust 提交被取消或跳过后，后续文档提交仍包含未通过的 Rust 差异。查询只读最近 100 条成功运行，排除当前运行、相同 HEAD、其他分支和事件；无可信祖先、API 失败、无 token 或历史不匹配时选择全部核心检查，绝不退回事件的 `before`。CI 规则文件本身的变化也会选取全部核心检查。

计划冻结 `coverage_base`、`coverage_head` 和原因。`ci-gate` 在完整 Git 历史中核对当前提交及事件，用冻结范围重新生成路径和检查选择，不再次查询成功运行。定时、手动和 merge queue 保持各自的全范围规则。本地显式 `--base`、`--head` 使用指定范围，不查询 GitHub。

| 改动 | 核心检查 |
| --- | --- |
| `README.md`、`README.zh-CN.md`、`AGENTS.md`、`CLAUDE.md` | 静态快检，不构建或运行 Rust 测试 |
| `apps/www/`、`packages/docs/` | 门户类型检查、静态构建和产物检查，不运行 Rust 测试 |
| `apps/app/`、Web 构建脚本 | 浏览器密码学互通、Web 测试与类型检查、静态构建、Linux x64 完整包及网页进程验收 |
| 共享主题、根 JS package、pnpm workspace 或 lockfile | Node Web 检查与门户检查 |
| Node Rust、迁移、SDK、安装、打包或进程验收脚本 | Node 快速测试、Clippy、Web 和 Linux x64 制品启动验收 |
| Bridge | Bridge、Control、CLI 和 Node 的核心检查 |
| Protocol | Protocol、Bridge、Control、CLI 和 Node 的核心检查 |
| Secure、密码学验收脚本 | Secure、Secure FFI、Node 和浏览器密码学互通 |
| Delivery 领域库、proof、合约和服务入口 | Delivery 与 Node 核心检查、新 guest 的公开证明验证和本地 EVM 验收 |
| Cargo manifest、`Cargo.lock`、工具链、CI 规则、未知输入 | 整个工作区的核心检查 |

静态快检保留 Rust fmt、工作流与 shell 检查、JavaScript 语法、选择器和安装器自检。涉及 Rust 时校验 Atlas 迁移文件，不打开数据库。快速 Rust 测试保留权限、个人网络隔离、数据持久化、写操作去重与未知结果对账等已有回归，只跳过 `SLOW_TESTS` 明确列出的重型用例，目前是 10 万事件追加测试和超出待补依赖上限的 4 万事件追平测试。

`www` 使用 Node.js 24 和冻结的 pnpm 依赖，依次运行 `typecheck`、`build`、`check:static`，上传 `apps/www/dist/client` 为 `www-<SHA>` 制品。门户与独立 App 分别构建；根 `packages/docs/` 是门户文档正文的唯一来源，修改文档也会触发 `www`。

首个正式版前按 `AGENTS.md` 保持 `DATA_EPOCH=1`，不随功能或 Nightly 递增。改 `apps/core/migrations/` 或 `apps/core/src/delivery/schema.sql` 时直接维护最终 schema，并更新 `apps/core/src/config.rs` 的 `DATA_EPOCH_SCHEMA_SHA256`。当前检查比较 schema digest，不证明 epoch 增加。确需调整 epoch 时须单独说明理由，并同步 `crates/control/src/lib.rs`、`install.sh` 与 `ci.py --self-test`；不以新增迁移或兼容回退代替最终数据形状。

Web 路径优先于 Node 路径，只有 `packages/client/src/avatar/face.ts` 与 `derived-faces.json` 例外：它们按 Node 路径选取，因为这组向量也是 Node Rust 测试的输入，核对 Node 由 bot ID 派生的长相与客户端一致；Node 受影响时 Web 检查照常运行。Node 对 Protocol、Bridge、Secure 的依赖用于向上扩展检查范围。PR 按 `merge-base(base, head)..head` 的完整差异选取，删除与重命名包括旧、新路径。无法识别的输入选择整个工作区核心检查。

`delivery` 路径先于通用 Node 和 Cargo 路径匹配。它包括 `crates/delivery/`、`crates/delivery/proof/`、`contracts/delivery/`、`tests/fixtures/delivery-probe/`、`apps/core/src/delivery/`、发现实现、服务入口与本地钱包边界，以及两个 delivery 验收脚本。Delivery 领域库向上触发 Node 检查，因此制品验收所需的 Web 和密码学资源仍会构建。完整工作区检查也要求 `delivery` 任务成功。

Linux 制品核对 checksum、源码提交、版本、target、WASM 和存储 epoch，再以 `verify-node.py --web-only` 启动真实进程，检查网页资源、SPA 路由、CSP、缓存与 404。它不执行多 Node 业务验收，也不验证浏览器渲染。

Linux 制品还使用 [e2e](e2e.md) 启动隔离 Node，在 Chromium 中验证建网页面以及建网、下载恢复文件、跳过 AI 设置、创建 Bot 和刷新后保留的流程。它不调用真实模型。CI 上传浏览器测试报告，排除下载的恢复文件和 Node 数据。

## 可验证交付的核心检查

`delivery` 任务使用 Rust 1.98.1 构建独立的 `crates/delivery/proof` host。guest 编译器固定为 RISC Zero Rust `r0.1.97.0`，由 `scripts/install-delivery-toolchain.py` 校验官方归档 SHA-256 后安装到 `.artifacts/vdp/risc0/`。它不改全局 Rust 配置，Node 默认构建不依赖重型 proof host。

规范 guest 只在 Linux x86_64 构建。CI 把源码、Cargo 缓存、编译器和产物分别挂到 `/work`、`/cargo`、`/risc0` 和 `/artifacts`，再调用唯一入口 `crates/delivery/proof/build.sh`。固定路径和 guest 的 remap 参数避免把不同机器的绝对路径写入不同的 image。Mac 原生构建不能作为该规范 image 的证明验收。

`scripts/check-delivery.py` 读取 `crates/delivery/proof/fixtures/manifest.json` 和四种关系的公开 Groth16 receipt。它通过规范入口重新编译 guest，核对 image ID、ELF SHA-256、唯一 `Statement.sol` 的 schema hash 和 47 槽 journal，再用本次构建的 host 验证 receipt。验证还要求 host 导出的 Groth16 seal 与对应公开 fixture 逐字一致，拒绝缺失或额外字段。旧 image、过期 schema、错误 journal、缺文件、dev mode 和跳过 guest 构建都会失败。

当前仓库已包含四种关系的九个核心公开 fixture。旧 Strict/十三证明 live 流程已退役；当前受保护两单最小样本须另行产生一份 KeyBinding、各单 Source/Hold，共五份真实证明。默认检查会重建 guest 并验证这些 fixture，目录不存在时打印 `skipped: no public proof fixture`，跳过 receipt 验证和 `verify-delivery.mjs`，其余检查照常运行；目录存在但内容不全仍然失败。新的证明只在固定的 x86_64 sandbox 中按 11 GiB、零 swap 和 64 PIDs 的资源边界生成，文件较小不保证 Groth16 压缩所需内存较小。

所有 receipt 还必须绑定产品的 `crates/delivery/proof/profile.json`。检查要求该文件与重编 guest 的 image、schema 相同，并固定为两个窗口、每窗 65,536 字节、总共 131,072 字节、64,000,000 cycles 和 16 GiB 内存上限。Rust fixture 命令显式读取该文件，以同一 ABI 生成 profile ID，再逐项核对 receipt；默认的 16 窗口测试 profile 不属于产品信任根。guest 不读取产品 profile 文件，避免 image 自引用。

合约检查使用锁定的 solc、Ganache 和 ethers。官方 verifier 源码逐项校验摘要，`compile.mjs --check-trust` 对比重新编译的 runtime 与已提交的 `trust.json`，不改 tracked 文件。检查还包括账务正反例、官方 published fixture、两连续窗口的 Rust 与 Solidity 编码比较，以及当前 delivery image 的真实 seal 在生产 adapter 上的验真和篡改拒绝。

默认 `run.mjs` 已迁移为受保护预算账务正反例，覆盖真实两订单批次、不可变预分配、奖金、固定收款、earned 幂等、退款与异常代币；使用显式 test digest verifier，不生成或冒充 genuine proof。`exercise-settlement.mjs --self-test` 同时检查 schema2 公开证据边界。旧 voluntary 通道与签券入口已撤下；`--delivery-charity` 仍不启动链或钱包。

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

`--sandbox` 先检查 fixture，再用固定的 `crates/delivery/proof/Sandbox.Dockerfile` 构建本地镜像。构建上下文仅含 Dockerfile 和 gnark 启动脚本。容器只挂四个目录，复用 `.artifacts/vdp/cargo`、`.artifacts/vdp/risc0` 和 `.artifacts/vdp/canonical`，不挂 Docker socket，不把 host 凭证或 dev mode 作为参数或挂载传入容器。内存上限为 11 GiB，内存与交换合计也限 11 GiB，最多 64 个进程。容器先在内部安装固定 guest 编译器，再执行默认完整检查。Preflight proof 域使用此入口，Mac 不安装原生 guest 编译器。

`--check-fixtures` 只核对公开文件结构和 schema，不验证证明。两个脚本的 `--self-test` 也只验证工具自身的正反例。整个 fixture 目录不存在时按上文跳过 receipt 验证；fixture 不全、缺编译器、依赖或 verifier 时退出失败，不能把未验证结果记为通过。

普通 PR 和扩展 CI 不生成新的昂贵证明，也不为新容量或资源上限背书。旧 `delivery_live` Strict/十三证明流程明确拒绝启动，不能用其成功状态为新预算背书；受保护真实 Node/证明验收须独立执行。修改密码关系或 guest 后，先验收新 image 的真实证明，再更新公开 fixture。生产 adapter 的 seal 验真只能证明该公开关系，不能单独证明托管合约已经收付款或用户已读。

当计划选中 `delivery` 时，Linux x64 package 任务还以本次打包的 Node 和 JS/WASM 执行 `verify-node.py --delivery-charity`。它先以 `--locked` 构建独立的 `tests/fixtures/delivery-probe`，产物放在 `.artifacts/delivery-probe/target`。probe 编译时包含当前产品的发现实现，进程验收重新核对该源码摘要。公网角色、不同个人网络的 Node、真实 loopback QUIC/Noise、免费发现、公益文件恢复及私人协议拒绝使用真实进程验证。CI 仅上传该模式的公开 `report.json`。

公益进程验收与 paid proof 任务各自必须通过。公益收据不代表链上交付或结算。Preflight 对 probe 和相关服务改动单独运行 fmt 与最小 locked build，不把这一构建当作进程验收。

## 手动生成真实付费交付证据

`delivery_small` 是默认关闭的独立小实验。显式选中后只运行 `delivery-small-prepare`、四关系证明 matrix 和 `delivery-small-accept`，跳过常规计划与 aggregate gate，不构建 Android 或运行 full live。两个实验选项不能同时选中。该次手动运行的成功不替代普通 PR 的最终 HEAD 检查。

```sh
gh workflow run ci.yml --ref <实验分支> -f delivery_small=true -f delivery_live=false
# 从明确的同源运行续用已合格的公开分项。
gh workflow run ci.yml --ref <同一提交的分支> -f delivery_small=true -f delivery_live=false -f delivery_small_resume_run=<run-id>
# 本机只运行准备控制，不生成证明或可复用分项证书。
python3 scripts/check-delivery.py --sandbox --prove-small prepare
```

`--prove-small` 必须指定 `prepare`、`relation-1` 至 `relation-4` 或 `accept`，不保留旧的无参数四证明入口。证明与续跑项要求真实 Actions 来源。`--resume-run` 仅供宿主选择一个明确的旧 run，不猜测分支最新运行。内部 `--resume-dir` 指向宿主已按 API、精确文件树和摘要检查的私有候选集合；容器中的 Python 用当前重编 host 验真全部合格候选后，才生成 Node 可读取的规范化目录。GH token 留在宿主，不进入容器环境、envfile 或新增挂载。

小实验保持原来的 native Ubuntu x86_64 sandbox、11 GiB 内存、零 swap、64 PIDs、segment po2=20 和产品 profile。整个 small workflow 使用跨 ref 的串行 concurrency group，matrix `max-parallel: 1` 且 `fail-fast: false`。每项在 Docker build 前设置唯一绝对截止，所有子命令共用它，截止早于 hosted job 的六小时硬限，留出终止、清理和上传余量。该限制不保证任一证明能完成，也不续用证明中间的私密 checkpoint。

`prepare` 使用封闭的公开测试部署配方，真实重建两个全新链与同一 4096 字节订单，逐字比较部署公开状态及四份 journal。它还完成四次 execute、八次空 stdin receipt decoder 控制和旧 image 的 PreTerms 反例。这些是非证明控制。四个证明项各自重建材料，只生成所选关系的一份真实 Groth16 receipt，或独立验真该关系已保存的合格分项。`accept` 不生成证明，先要求四份合格分项，再重放同一实际部署，完成正常 adapter 和原三条资金、恢复分支。缺项显式失败，不启动资金操作。

公开 context 绑定当前干净 HEAD、实际 guest ELF/image/schema、原 profile 字节摘要、部署交易与区块、基线余额、订单和四份 journal。源码改变后不能自动续用旧分项。宿主通过当前仓库 Actions API 核对 workflow path、`workflow_dispatch`、head SHA、expiry、精确 artifact 名称、不可变 artifact ID 与 digest。允许同源 failed-job 重跑组合不同 producer attempt 的分项，不要求所有分项来自最新 attempt。相同关系的竞争候选全部独立验真后按固定来源顺序选择；冲突或声称合格但损坏的候选导致失败。只保留原 producer 与原证明资源，当前验证资源和导入 artifact ID 单独记录，不累计前驱图。

同一 Held 快照分别验证正常 revealBatch、可选 Open 与 withdrawBudget 的 17 atoms、pending E 后超期失败并退款、成功 publish 后受控回滚并退款。每条分支记录实际 Token 余额、事件、交易回执和原生 gas，并由独立恢复程序只接收 Hold 与交易中实际观察到的 E，核对原文件字节和 SHA-256。公开固定的测试种子不为生产保密背书。恢复程序没有链参数，这不等于操作系统网络或文件访问隔离。快照分支不能相加为最终链上收入，Ganache 回滚不代表公网共识攻击。

结果在 `.artifacts/vdp/canonical/small/<work-id>/public/`，每个阶段单独上传精确白名单。`context.json`、`summary.json` 和 `resources.json` 之外，合格关系项只有 `partials/relation-N/part.json` 及实际 `.json`、`.receipt` 两个证明文件。`VERIFIED_PARTIAL` 要求当前 host 的二进制正验、Node 单一构造的 intended wrong-journal 拒绝、全部控制、Node 清理和最终 cgroup 无 OOM 观察通过。Node 后续失败或资源 UNKNOWN/OOM 时，只能保留当前 host 已按实际 profile/context 独立验真的证明对，标为 `UNQUALIFIED`，不写可复用证书，也不满足缺项。未验真的字节永不上传。prepare 的 `PREPARED` 和本机 `local-control` 均不代表证明成功。

完整验收候选通过独立二进制检查后，`proofs/` 恰好保留原四关系的九个文件，manifest 和核心 loader 不放宽。最终资源检查失败仍判整轮实验失败，但保留此前已独立验真的完整 bundle，摘要将 `VERIFIED_BUNDLE` 与整体 `FAIL` 分开。公开目录和文件分别为 `0755/0644`，私有目录和文件保持 `0700/0600`。原文、恢复文件、E、witness、接收秘密、私钥、vectors、Succinct checkpoint、calldata 与原始日志均不上传。CPU 是整个 sandbox 区间增量，`memory.peak` 是容器生命周期累计峰值，复用验证耗用不替代原证明耗用。任何证明、结算、恢复、清理或资源检查失败都不能报告完整实验通过。

旧 `delivery_live` 与 `verify-node.py --delivery` 依赖 Strict/十三证明和逐单 fund/publish，已明确拒绝其原启动协议。相关历史 artifact/控制器纯自检可继续用于识别旧证据，但不运行当前受保护付费，也不计入当前产品完成项。该手动选项不得被当成新预算验收；普通核心 CI 不自动生成新证明。

当前预算真实进程入口为 `run-delivery-live.py --protected-config <0600-final-config>`，内部调用当前 `verify-node.py --delivery-protected`。它要求 clean HEAD、精确 Linux CLI/Core/crypto/host/r0vm/profile/ELF 摘要与原资源硬界，新建五份 genuine receipt、两笔订单和三种真正2slot批次，并核对自动提款与真实重启。详细配置见 [受保护交付验收](e2e.md#验证受保护付费交付)。`run-delivery-live.py --protected-selfcheck` 不启动链、Node、套接字或证明，不等于执行该验收；该模式没有自动接入旧 `delivery_live` job，也不替代独立渲染证据。

当前受保护验收通过正式产品入口完成：两个 paid/recoverable 单窗口订单、同一预付预算、`confirm_batch`、P 的 `ready_windows` 与显式 `budget flush`。实际 reserveBatch、holdBatch、revealBatch 均须至少一笔含两个不同 slot；S 上传完必要对象并离线后，P 独立 earned/withdraw，R 实际恢复原文件。新的最小五份真实证明驱动、真实重启、同 HEAD 制品及界面渲染需分别验收，不能用合约探针、旧17 fixture 或 parser 自检替代。正式公开 bundle 使用 schema2，`observe-settlement.mjs` 只读原链，核对 pinned runtime、逐窗 genuine Source/Hold、同 canonical anchor 的预算与订单、实际方法/事件/金额/余额和 gas。binary receipt 还必须由官方 host 独立验真。

首次 E 外发门、签名私密状态、DisclosedOutstanding 近截止恢复、nonce-consuming revert 后原 cap 新 attempt、锁钱包/关实验室的只读对账都属于原生安全验收。固定本地 policy、两次确认或 cached genuine17 不构成公网活性、真实稳定币或绝对公平证明。见 [受保护预算说明](paid-delivery.md)。

## 定期、手动和发布检查

定时检查每 4 小时运行一次，使用 `23 */4 * * *`，每日 UTC 00:23、04:23、08:23、12:23、16:23、20:23 发起。定时和 Actions 中手动运行 `ci` 强制执行扩展检查，不受路径影响。merge queue 执行整个工作区的核心检查，不启用慢测试、MSRV 或三平台打包。开发分支 PR 不触发 `ci`。产品 `v` Tag 由 `release.yml` 并行构建全部产品目标，`www-v` 独立构建官网。`ci` 的手动入口只验证和生成制品，不发布。在 GitHub Actions 选择 `ci` → `Run workflow`，选择待验收分支后运行。

每四小时 Nightly 接在同一 schedule run 的 `ci-gate` 后，不新增第二套定时全量 CI。`daily-nightly` 仅接受 main 的 schedule，以显式 `!cancelled()` 和 `ci-gate` success 作为条件，允许上游不适用的检查正常跳过。协调器使用固定 concurrency 且不取消进行中的协调。启用需要明确授权未来每四小时机械版本 PR 自动 squash，并配置专用单仓库 `NIGHTLY_RELEASE_TOKEN`；当前实现不代表凭证或自动发布已启用。该 PAT 需 Contents、Pull requests、Actions 所需权限，只处理版本 PR 创建和普通 squash；版本 branch、tag 与精确 workflow dispatch 使用 job 级 `GITHUB_TOKEN`。未配置时明确失败，不修改权限或保护规则。

协调脚本先验证精确机械版本 diff 和 `cargo metadata --locked --no-deps`，合并后只接受真实 squash SHA 的 main push CI success，再并行启动产品与官网发布组。不会为版本分支再派发一轮全量检查，也不为合并 SHA 再跑扩展全量 CI。原 run 的 `created_at` 转为 UTC 后向下取整到四小时批次，使用 `bot/daily-nightly/YYYYMMDDHH` 分支；版本日期保留 `YYYYMMDD`。不同批次分配新 PR 和递增版本，Android versionCode 与 iOS buildNumber 各递增一次；延迟启动或跨日重跑复用原批次 PR、版本、构建号、SHA 和留存制品。旧批次若尚无分支与 PR，而 main 的产品或官网 Nightly 日期已更晚，则拒绝初始化，避免版本倒退。quick-check 与 preflight 注册 `python3 scripts/daily-nightly.py --self-test` 及 Python 语法检查，涵盖六个 UTC 批次、同批次恢复、同日递增、旧批次拒绝倒退、真实临时 Git、篡改 diff、完整产品发布门、未知写入对账和原 artifact 恢复。具体发布顺序与停止条件见 [每 4 小时 Nightly 的发布约束](releasing.md#每-4-小时-nightly-的发布约束)。

| 扩展检查 | 执行范围 |
| --- | --- |
| Rust 快、慢测试及 Clippy | 整个工作区；慢组为精确名称的 `SLOW_TESTS` |
| 最低 Rust 版本 | 安装 Cargo 声明的 MSRV，检查整个工作区 |
| 原生 release 制品 | Linux x64、Linux ARM64、macOS ARM64、Windows x64，分别校验身份；Windows 使用原生进程验收 |
| 单 Node 调度与事件日志验收 | Linux x64 一次，使用 `--phase1` |
| 多 Node 同步、入口与投递验收 | Linux x64 一次，不带模式参数，四台 Node 经 iroh 和成员 relay 互连 |
| 聊天 Agent 模拟协议验收 | macOS ARM64 一次，使用 `--chat-agent` |
| 跨设备协作、换负责人与附件验收 | Linux x64 一次，使用 `--phase4` |
| 公共入口与 relay 令牌验收 | Linux x64 一次，使用 `--public-entry` |
| 仅发现、普通 relay 回退与无网页验收 | Linux x64 一次，使用 `--discovery` |
| 写入保护、状态与诊断包验收 | macOS ARM64 一次，使用 `--phase5` |
| Nightly npm 包 | 组装四个原生包，四个平台分别安装并检查版本，POSIX 平台另执行 `--web-only` 冒烟 |

快、慢测试组均使用 `opt-level=1`，保留调试断言和整数溢出检查，调试信息使用 `line-tables-only` 保留回溯文件和行号，共享独立的 `test-opt1` Rust 缓存；Clippy 使用单独的 `check` 缓存。两组运行前校验每个慢测试名恰好匹配一个已有测试。慢组用 `--include-ignored` 运行列出的用例，其他 `ignored` 用例仍默认忽略。两组测试命令用 `/usr/bin/time -v` 记录 CPU 与 maxRSS；maxRSS 不能当作并发进程树的总峰值。

CI 不使用真实 AI Provider 或模型密钥。各模式都使用受控 SDK 和 App Server 夹具，`--phase5` 不启动 App Server。模拟协议通过不能证明真实模型理解需求或正确选择工具。

产品 `v<version>` 与官网 `www-v<version>` 分别进入发布路径，必须匹配声明版本与 main 中的固定源码。产品并行构建 Web、四个原生目标、Android 和 iOS，各平台再按自身输入组包并构建桌面 App。每组校验后立即向同一个公开 Release 追加附件，完整审计完成后才追加全局 `release.json`。`publish=false` 手动入口只验证并生成制品。产品与官网制品发布不触发部署；独立部署命令可显式选择已发布的 Stable、RC 或 Nightly Tag；派发成功不代表部署成功。详见 [构建与发布](releasing.md)。

Windows 运行包的定位可复用已完成 CI 中成功的 Windows package 制品，单独运行当前验收脚本：

```sh
gh workflow run ci.yml --ref <branch> -F windows_only=true -f windows_package_run=<run-id>
```

入口核对原运行、成功生产者、不可变 artifact ID、来源提交、有效期和文件摘要。诊断收据分别记录运行包与验收脚本的源码，并使用独立 artifact 名称。该模式跳过 Rust 编译与其他平台构建；最终交付仍需用最终提交构建并验收完整产品集。

## 本地按改动补充验收

开发者修改同步、恢复、调度或工具执行时，除 PR 核心检查外，必须执行对应专项并记录命令及结果；不能只依赖下次定期运行。每次只跑相关模式。安装器或平台交付改动应额外手动触发 `ci`，确认目标平台制品。

Web 开发使用 `pnpm --filter vibeface-web dev` 或 `pnpm --filter vibeface-www dev`，Vite HMR 只更新开发页面。Rust 可按需复用 Cargo 增量缓存，依赖与编译结果仍以 Cargo 为准，不引入另一套构建调度或 Core 热重载。最终验收使用当前提交的真实制品并重启进程，核对 Node、scheduler、Provider、DAG 和持久化状态的恢复、去重与未知结果对账。

无需编译项目即可验证 CI 规则和发布隐私检查：

```sh
python3 scripts/ci.py --self-test
python3 scripts/ci.py plan --base origin/main --head HEAD --event pull_request
actionlint .github/workflows/ci.yml
```

计划 JSON 的 `scopes`、`rust_packages`、`required_jobs` 和冻结范围说明本次选取依据。`local_process_modes` 是需要另外执行的 `verify-node.py` 模式建议，按重叠路径取并集并去重，不会自动启动专项。`default` 表示不加模式参数。文档或纯 Web 改动返回空列表，未知 Core 或共享基础改动保守建议全部常规模式；涉及 Delivery 时另列免费 `delivery-charity`，不会隐式加入付费 `delivery`。GitHub 摘要也列出对应命令，建议本身不代表验收通过。

已有 `--plan-env` 可复用同一个本地计划运行选中的 Rust 包。空包列表时跳过 Rust 命令：

```sh
set -e
CI_PLAN="$(python3 scripts/ci.py plan --base origin/main --head HEAD --event pull_request)"
export CI_PLAN
if python3 -c 'import json, os, sys; sys.exit(not json.loads(os.environ["CI_PLAN"])["rust_packages"])'; then
  python3 scripts/ci.py check --plan-env CI_PLAN
  python3 scripts/ci.py test --tier fast --plan-env CI_PLAN
fi
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

`delivery-charity` 还需要当前提交的 probe。按 `--locked` 构建后单独执行，不启动付费证明或真实资产操作：

```sh
VIBEFACE_PROBE_SOURCE_ROOT="$PWD" CARGO_TARGET_DIR="$PWD/.artifacts/delivery-probe/target" cargo build --manifest-path tests/fixtures/delivery-probe/Cargo.toml --locked
probe="$PWD/.artifacts/delivery-probe/target/debug/vibeface-delivery-probe"
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-delivery-charity --delivery-charity --delivery-probe "$probe"
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

Rust 快、慢组在计划生成后直接开始，Clippy 另等待静态快检。它们不等待或下载 Web 构建产物。制品任务继续等待 Web 和密码学资源，并执行原有的源码身份、真实进程和浏览器验收。

## 缓存与并发

`ci` 按事件类型和 Git ref 分组，仅 `main` push 的首次运行自动取消旧的同组运行，累计覆盖范围保留旧运行尚未通过的改动。每次手动运行和 `run_attempt > 1` 的重跑再按 `run_id` 分组，因此重跑旧提交不会挤掉新提交的检查，显式 `delivery_live` 不会被后续 push 取消，也不会被另一条手动运行替换其 pending 位置。定时和 merge queue 不取消正在执行的同组运行，同组仍最多保留一个等待运行。主分支核心检查可以与定时完整检查同时进行。应用 tag 不触发普通 `ci`，发布与部署工作流使用各自的并发规则。 `delivery_small` 仍使用前文的跨 ref 串行分组，不按 `run_id` 拆组，也不取消正在执行的实验。

Linux x64 制品任务使用 pnpm 缓存，并按 runner、架构和 `pnpm-lock.yaml` 缓存 Chromium 二进制。浏览器的系统依赖从 Ubuntu 官方镜像下载，沿用签名仓库配置。每次仍执行冻结依赖安装、浏览器与系统依赖安装以及完整浏览器用例。缓存只包含浏览器文件，不包含测试结果、Node 数据或下载的恢复文件。

`ci` 工作流中的 Rust 缓存仅在 `refs/heads/main` 或显式 `workflow_dispatch` 时保存。手动运行可为相同 ref 的后续运行预热。新增 profile 或缓存 key 在首次保存前需要冷构建。应用 tag 使用独立发布工作流，不属于本节缓存保存规则。

Rust 缓存命中不表示当前 workspace 无需编译。每次仍由 Cargo 核对源码和构建参数；评估缓存时同时记录编译、保存与还原的耗时。

当前 pinned `rust-cache` 主要保存第三方依赖，未启用 `cache-workspace-crates`。单独打开该选项仍不能保证新 checkout 复用本项目产物：Cargo 会因源码文件时间更新而重编，而且精确命中的不可变缓存不会追加保存。这里保留现有配置，不通过改写源码时间绕过 Cargo 的失效检查。

Android 使用官方 Gradle 缓存动作和 Gradle build cache，保留 `arm64-v8a` 与 `x86_64` 的 release 构建。手动运行可以写入其 Git ref 的缓存。缓存命中仍需执行原构建和制品检查，不能替代本次提交的验收。

## 合并门禁

`ci-gate` 要求选中的任务成功，选中任务失败、取消、缺失或跳过均拒绝通过。未选中的任务允许跳过或缺失；若它们实际报告失败或取消，仍拒绝通过。gate 核对冻结提交范围并重新计算计划，拒绝缩小 `scopes`、必需任务或其他选取结果。它服务于 main、merge queue、定时和手动运行，不是开发分支 PR 的必需检查。

GitHub 主分支规则不要求 PR 先更新到最新 base，也不将 `ci-gate` 列为必需状态。仓库开发流程仍要求相关本地验证、最终 HEAD 的 preflight 和明确合并授权。合并后的 CI 与应用 tag 的制品、发布和部署检查分别记录。

产品与官网 Tag 由 `release.yml` 独立校验和发布；main 的普通 CI 不公开产品。参见 [按产品组合发布](releasing.md#按产品组合发布)。


## 桌面专项验收

在开发分支手动运行已注册的 `ci.yml`，设置 `desktop_only=true`。共享密码学与 Web 输入构建后，Mac ARM64、Windows x64、Linux x64 和 Linux ARM64 各自编译、组包并驱动真实 Electron 窗口，一端不等待另一端的原生构建或测试。独立的 `windows-platform` 按 Core、Bridge 和 CLI 并行执行 MSVC Clippy 和 Windows Rust 测试，一组失败不会中止其他组。Linux 用例在 Xvfb 中启动实际 AppImage 解包后的 App，保持 Chromium 沙箱开启。最终 `desktop-gate` 要求所有这些任务成功。

```sh
	gh workflow run ci.yml --repo leaperone/vibeface --ref <branch> -f desktop_only=true
```

只定位 Windows Rust 问题时，使用 `windows_only=true`。这条路径不启动桌面构建，其 `windows-gate` 只表示原生 Windows Rust 检查通过，不代表完整桌面验收。

桌面测试使用打包应用自身的 DevTools 页面，核对真实 Electron 版本、窗口隔离、内置包来源、自有 Core、退出和重启。测试预先用包内 CLI 建立关闭公共引导的独立 Node，随后由真实启动器启动并持有 Core；首次初始化逻辑另由桌面会话测试覆盖。Windows 另外核对真实 SID/DACL、维护通道、Unicode 路径、Provider 子进程回收和当前用户服务。Unix shell、Unix 权限位及 Linux 证明宿主夹具保留在 Unix 测试中；协调器的协议测试在两端使用各自的私有 IPC。所有 Provider 均为受控夹具，不调用真实模型。


原生移动端构建可使用 `ci.yml` 的 `mobile_apps=ios|android|both`。`both` 让两个系统并行构建，`mobile-gate` 只验收选中的系统。iOS 设备归档验证不等同于模拟器界面或真机安装验收；现有 `mobile.yml` 保留模拟器界面测试。手动移动端验收按运行编号隔离并行，避免新提交取消尚在验证的专用模拟器任务。缺少 iOS 分发身份时，未签名设备归档的收据必须保留不可安装状态。
