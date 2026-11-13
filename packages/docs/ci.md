---
title: 按改动选择 CI
description: 查看 Vibeface CI 如何按改动范围选择检查。
---

# 按改动选择 CI

`.github/workflows/ci.yml` 使用 `scripts/ci.py` 选择检查范围、Rust 测试分组和必需任务。常规 CI 运行生成计划，`ci-gate` 要求选中的任务全部成功。没有顶层 `paths` 筛选。

开发分支的 PR 不触发 GitHub Actions，也不要求 CI 状态；需要远端验证时手动运行 `CI / Core`。Actions 只保留核心 CI、发布和部署入口：桌面四平台、Windows native、移动端构建与验收、原生诊断和付费交付实验入口已移除，iOS 模拟器等界面验收不在 CI 中运行。`CI / Core` 由 `main` push、手动运行或 merge queue 触发，不含定时入口。产品 tag 交给 `release.yml` 构建完整产品，官网 tag 交给 `release-www.yml`；完整 Release 发布后由对应发布工作流自动派发部署。合并、CI 通过、发布和部署各有自己的验收结果。

## 按用途选择 Actions 入口

下表描述本分支的工作流定义。新增手动入口需合入默认分支后，才可在 Actions 页面或 CLI 中派发。

| 文件 | Actions 名称 | 触发方式 | 用途 |
| --- | --- | --- | --- |
| `ci.yml` | CI / Core | main push、merge queue、手动、复用调用 | 自动按累计差异选核心检查；手动与复用调用执行扩展检查 |
| `hourly.yml` | Release / Hourly | 每小时、手动 | 为最新 CI 通过的 main 提交创建当小时 Hourly Tag 并派发产品发布；不运行 Core 检查 |
| `release.yml` | Release / Product | `v*` push、手动 | 固定产品组；完整发布后自动派发 Node + App，手动默认只构建 |
| `release-www.yml` | Release / Website | `www-v*` push、手动 | 固定官网组；完整发布后自动派发官网，手动默认只构建 |
| `npm-publish.yml` | Release / npm Hourly | 产品发布调用、手动 | 验证完整产品发布的原始 npm 制品；手动默认只验证 |
| `deploy-node.yml`、`deploy-app.yml`、`deploy-www.yml`、`deploy-product.yml` | Deploy / Node、Web App、Website、Product services | 自动派发或手动 | 固定目标部署，`tag` 必填；手动运行 `apply=false` 默认构建验证与排练 |

`_ci-crypto.yml` 与 `_ci-web.yml` 仅供复用，不是手动入口。部署入口统一调用 `deploy-services.yml`，由它持有全局部署锁。`ci.yml` 的 `package_policy` 仅供复用调用，允许 `standard` 或 `linux-native`；公开手动 Core 入口没有模式参数。

| 反馈层 | 入口 | 结果含义 |
| --- | --- | --- |
| 编辑反馈 | Web 的 Vite HMR、按需 Rust 增量构建 | 辅助开发，不证明重启、恢复或制品可用 |
| 本地相关检查 | `ci.py plan`、选中包的检查与进程专项 | 核对本次改动涉及的行为 |
| PR 验收 | 独立工作树和同一个 PR，创建后及最终 HEAD 执行 `preflight --no-merge` | 检查固定 base、HEAD、范围及必要证据，不自动合并 |
| 合并后检查 | 经明确授权合并，再核对 `main` 的 `ci-gate` | 证明该运行选中的检查成功，不发布应用 |
| 制品发布 | 显式产品或官网 Tag 与各自发布入口 | 校验产品制品和发布记录；完整 Release 发布后自动派发部署 |
| 服务部署 | 自动派发或独立 deploy 命令与对应部署 workflow | 消费已发布版本，核对实际运行版本和服务行为 |

## main 与 merge queue 的核心检查

开发分支 PR 不运行本节检查。`main` push 按路径运行核心检查；merge queue 无路径筛选，执行整个工作区的核心检查。

自动 `main` push 从同仓库 `ci.yml` 最近的成功 main push 运行中选择已验证的祖先提交，以该提交到当前 HEAD 的累计差异选取检查。较早的 Rust 提交被取消或跳过后，后续文档提交仍包含未通过的 Rust 差异。查询只读最近 100 条成功运行，排除当前运行、相同 HEAD、其他分支和事件；无可信祖先、API 失败、无 token 或历史不匹配时选择全部核心检查，绝不退回事件的 `before`。CI 规则文件本身的变化也会选取全部核心检查。

计划冻结 `coverage_base`、`coverage_head` 和原因。`ci-gate` 在完整 Git 历史中核对当前提交及事件，用冻结范围重新生成路径和检查选择，不再次查询成功运行。手动、复用调用和 merge queue 保持各自的全范围规则。本地显式 `--base`、`--head` 使用指定范围，不查询 GitHub。

| 改动 | 核心检查 |
| --- | --- |
| `README.md`、`README.zh-CN.md`、`AGENTS.md`、`CLAUDE.md` | 静态快检，不构建或运行 Rust 测试 |
| `apps/www/`、`packages/docs/` | 门户类型检查、静态构建和产物检查，不运行 Rust 测试 |
| `apps/app/`、Web 构建脚本 | 浏览器密码学互通、Web 测试与类型检查、静态构建、Linux x64 完整包及网页进程验收 |
| `apps/app/src/lib/*.test.ts`、`apps/app/src/features/*/*.test.ts`，限定单层目录 | 浏览器密码学互通和独立 `web-tests`，不组包或构建静态页面 |
| `tests/e2e/` | 不选任何构建或测试；e2e 不在 CI 中运行，只在本地按 [e2e](e2e.md) 执行 |
| `apps/desktop/src/`、`apps/desktop/tests/` | Linux 上的原生 GPUI Rust 测试与 Clippy；真实窗口留在显式平台验收 |
| `apps/mobile/`、`packages/secure-native/` | 独立的 Mobile 与 secure-native 类型检查，同时保留 Web 套件中的手机行为测试 |
| `packages/client/`、共享主题 | Web 和 Mobile 检查；主题另运行门户检查 |
| 根 JS package、pnpm workspace 或 lockfile | Web、Mobile、桌面源码和门户检查 |
| Node Rust、迁移、安装、打包或进程验收脚本 | Node 快速测试、Clippy、Web 和 Linux x64 制品启动验收 |
| Bridge | Bridge、Rust Client、Control、CLI 和 Node 的核心检查 |
| Protocol | Protocol、Bridge、Rust Client、Control、CLI 和 Node 的核心检查 |
| Secure、密码学验收脚本 | Secure、Rust Client、Control、CLI、Node、Secure FFI 和浏览器密码学互通 |
| `crates/client/` Rust Client | Rust Client、Core 和 CLI 的核心检查 |
| Delivery 领域库、proof、合约和服务入口 | Delivery 与 Node 核心检查、新 guest 的公开证明验证和本地 EVM 验收 |
| Cargo manifest、`Cargo.lock`、工具链、CI 规则、未知输入 | 整个工作区的核心检查 |

静态快检保留 Rust fmt、工作流与 shell 检查、JavaScript 语法、选择器和安装器自检。涉及 Rust 时校验 Atlas 迁移文件，不打开数据库。快速 Rust 测试保留权限、个人网络隔离、数据持久化、写操作去重与未知结果对账等已有回归，只跳过 `SLOW_TESTS` 明确列出的超出待补依赖上限的 4 万事件追平测试。

`desktop-check` 在 Linux 上运行原生 Desktop 的 Cargo 测试与 Clippy，使用 Rust 1.98.1 和 hosted runner 系统库，不替代手动平台窗口验收。桌面安装配置、打包脚本及尚未分类的路径继续选择全工作区检查。

`web-tests` 执行与 Web 构建相同的 Web、共享 Client 行为测试及类型检查，下载并核对本提交的密码学产物。仍安装 Mobile 测试依赖。只有变更全部落在现有单测命令覆盖的路径时，才省去静态构建和组包；同时修改生产源码时由完整 Web job 承担这些检查，不重复运行测试 job。更深层测试、TSX 测试和公共 fixture 保持保守回退。原生 workspace 的手动验收要求保持不变，不把类型检查算作原生行为验收。

`mobile-check` 独立运行共享 Client、secure-native 和 Mobile 类型检查，`ci-gate` 要求它与选中的其他任务都成功。Web 构建脚本不再执行这两项原生类型检查；Web 套件仍包含手机行为测试，并读取手机依赖中的实现，所以暂时保留 Mobile 安装过滤器。移动这些行为测试前，不移除这部分依赖或手机变更对应的 Web 检查。全量事件和未知范围回退同时包含 Mobile、桌面源码检查。

`www` 使用 Node.js 24 和冻结的 pnpm 依赖，依次运行 `typecheck`、`build`、`check:static`，上传 `apps/www/dist/client` 为 `www-<SHA>` 制品。门户与独立 App 分别构建；根 `packages/docs/` 是门户文档正文的唯一来源，修改文档也会触发 `www`。

首个正式版前按 `AGENTS.md` 保持 `DATA_EPOCH=1`，不随功能或 Hourly 版本递增。改 `apps/core/migrations/` 或 `apps/core/src/delivery/schema.sql` 时直接维护最终 schema，并更新 `apps/core/src/config.rs` 的 `DATA_EPOCH_SCHEMA_SHA256`。当前检查比较 schema digest，不证明 epoch 增加。确需调整 epoch 时须单独说明理由，并同步 `crates/control/src/lib.rs`、`install.sh` 与 `ci.py --self-test`；不以新增迁移或兼容回退代替最终数据形状。

Web 路径优先于 Node 路径，只有 `packages/client/src/avatar/face.ts` 与 `derived-faces.json` 例外：它们按 Node 路径选取，因为这组向量也是 Node Rust 测试的输入，核对 Node 由 bot ID 派生的长相与客户端一致；Node 受影响时 Web 检查照常运行。Node 对 Protocol、Bridge、Secure 的依赖用于向上扩展检查范围。PR 按 `merge-base(base, head)..head` 的完整差异选取，删除与重命名包括旧、新路径。无法识别的输入选择整个工作区核心检查。

`delivery` 路径先于通用 Node 和 Cargo 路径匹配。它包括 `crates/delivery/`、`crates/delivery/proof/`、`contracts/delivery/`、`tests/fixtures/delivery-probe/`、`apps/core/src/delivery/`、发现实现、服务入口与本地钱包边界，以及 `check-delivery.py`、`install-delivery-toolchain.py`、`run-delivery-live.py`、`verify-delivery-protected.py` 和 `verify-delivery-restart-observer.py` 五个 delivery 脚本。Delivery 领域库向上触发 Node 检查，因此制品验收所需的 Web 和密码学资源仍会构建。完整工作区检查也要求 `delivery` 任务成功。

Linux 制品核对 checksum、源码提交、版本、target、WASM 和存储 epoch，再以 `verify-node.py --web-only` 启动真实进程，检查网页资源、SPA 路由、CSP、缓存与 404。它不执行多 Node 业务验收，也不验证浏览器渲染。

CI 不运行浏览器、桌面窗口或 iOS 模拟器 e2e，也不做截图类视觉验收。需要界面证据时按 [e2e](e2e.md) 在本地运行。

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

`--sandbox` 先检查 fixture，再用固定的 `crates/delivery/proof/Sandbox.Dockerfile` 构建本地镜像。构建上下文仅含 Dockerfile 和 gnark 启动脚本。本地 worktree 的 Cargo 目录（同时挂到 `/cargo` 和 `/home/runner/.cargo`）与 risc0 目录挂主 checkout 的共享 `.artifacts/vdp-shared/cargo`、`.artifacts/vdp-shared/risc0`，`.artifacts/vdp/canonical` 仍按 worktree 独立；非标准 checkout 退回本 worktree 的 `.artifacts/vdp`。完整检查开始前，若本 worktree 尚无 `proof-target`、`domain-target`，先从共享快照复制一份并把其中所有 mtime 置为 Unix epoch，迫使 path crate 和 guest 按本分支源码重建，registry 依赖保持热缓存；检查通过后再以整目录替换方式发布新快照，复制期间快照被替换则放弃并冷启动。CI 和 `--prove-small` 不做复制和发布。共享目录对 sandbox 容器可写，与宿主 cargo 本就运行分支代码的信任边界相同。guest 编译器在临时目录解压并写好收据后原子改名就位，并发安装时后到者校验并沿用已存在的安装。容器不挂 Docker socket，不把 host 凭证或 dev mode 作为参数或挂载传入容器。内存上限为 11 GiB，内存与交换合计也限 11 GiB，最多 64 个进程。容器先在内部安装固定 guest 编译器，再执行默认完整检查。Preflight proof 域使用此入口，Mac 不安装原生 guest 编译器。

`--check-fixtures` 只核对公开文件结构和 schema，不验证证明。两个脚本的 `--self-test` 也只验证工具自身的正反例。整个 fixture 目录不存在时按上文跳过 receipt 验证；fixture 不全、缺编译器、依赖或 verifier 时退出失败，不能把未验证结果记为通过。

普通 PR 和扩展 CI 不生成新的昂贵证明，也不为新容量或资源上限背书。旧 Strict/十三证明启动协议明确拒绝，不能用历史成功状态为新预算背书；当前受保护真实 Node/证明验收只由显式运行的宿主命令 `scripts/run-delivery-live.py` 执行，Actions 不再提供入口。修改密码关系或 guest 后，先验收新 image 的真实证明，再更新公开 fixture。生产 adapter 的 seal 验真只能证明该公开关系，不能单独证明托管合约已经收付款或用户已读。

当计划选中 `delivery` 时，Linux x64 package 任务还以本次打包的 Node 和 JS/WASM 执行 `verify-node.py --delivery-charity`。它先以 `--locked` 构建独立的 `tests/fixtures/delivery-probe`，产物放在 `.artifacts/delivery-probe/target`。probe 编译时包含当前产品的发现实现，进程验收重新核对该源码摘要。公网角色、不同个人网络的 Node、真实 loopback QUIC/Noise、免费发现、公益文件恢复、私人协议拒绝、每条服务连接只处理一个请求后关闭，以及停滞握手在限期内释放，都使用真实进程验证。CI 仅上传该模式的公开 `report.json`。

公益进程验收与 paid proof 任务各自必须通过。公益收据不代表链上交付或结算。Preflight 对 probe 和相关服务改动单独运行 fmt 与最小 locked build，不把这一构建当作进程验收。

## 手动生成真实付费交付证据

付费交付的真实证明实验（原 `delivery-small.yml` 与 `delivery-live.yml`）已从 Actions 移除，CI 不生成新证明，也不为证明容量或资源上限背书。本机仍可运行 `python3 scripts/check-delivery.py --sandbox --prove-small prepare` 生成不含证明的准备控制，以及 `scripts/run-delivery-live.py` 的宿主命令。要求真实 Actions 来源的证明分项与续用（`relation-1` 至 `relation-4`、`accept` 和 `--resume-run`）在恢复对应入口前不可用。受保护验收的配置与边界见 [受保护交付验收](e2e.md#验证受保护付费交付) 和 [受保护预算说明](paid-delivery.md)。

## 定期、手动和发布检查

`hourly.yml`（Release / Hourly）的 job `hourly` 每小时由 cron 触发，运行协调器 `scripts/hourly.py`。main 固定携带开发版本 `0.0.1-hourly.2026111300`；协调器选择 `ci.yml` push 运行已成功完成的最新 main 提交。该提交不包含最新 Hourly Tag 的 parent 时跳过；等于该 parent 且那次发布正在运行或已成功时也跳过。那次发布失败时用同一提交重建一次当小时新版本；同一提交第二次失败时协调器报错停止，等待修复或 main 的新提交。本 UTC 小时的 Tag 已存在时只核对来源，缺少发布运行时补派发。否则创建一个脱离 main 的 BOT 提交，只改版本字段：全部产品版本文件、`Cargo.lock` 中三个本仓包、Android versionCode（`YYMMDDHH`）与 iOS buildNumber（`YYMM.DD.HH`）。随后创建 `vX.Y.Z-hourly.YYYYMMDDHH` Tag 并派发 `release.yml`。协调器不等待发布结果，也不补发更早的小时；某小时失败时，下一小时发布新版本。详情见 [Hourly 发布](releasing.md#每小时-hourly-发布)。

手动 `CI / Core` 同样执行完整扩展检查，不受路径影响，只验证和生成制品。merge queue 执行整个工作区核心检查，不启用慢测试、MSRV 或四平台打包。开发分支 PR 不触发 Core。产品 `v` Tag 由 `release.yml` 构建全部产品目标，`www-v` Tag 由 `release-www.yml` 构建官网。

发布来源门：位于 main 的 Tag 要求同一 SHA 的 main push CI success；脱离 main 的 Hourly 来源必须恰好是机械版本提交，其唯一 parent 位于 main 且有 main push CI success。Stable/RC 不允许脱离 main 的来源。真正的 artifact source 与 provenance 仍是 Tag 的版本提交。quick-check 与 preflight 执行 `scripts/hourly.py --self-test`。

| 扩展检查 | 执行范围 |
| --- | --- |
| Rust 快、慢测试及 Clippy | 整个工作区；慢组为精确名称的 `SLOW_TESTS` |
| 最低 Rust 版本 | 安装 Cargo 声明的 MSRV，检查整个工作区 |
| 原生 release 制品 | Linux x64、Linux ARM64、macOS ARM64、Windows x64，分别校验身份；Windows 使用原生进程验收 |
| 单 Node 调度与事件日志验收 | Linux x64 一次，使用 `--phase1` |
| 多 Node 同步、入口与投递验收 | Linux x64 一次，不带模式参数，四台 Node 经 iroh 和成员 relay 互连 |
| 聊天 Agent 模拟协议验收 | macOS ARM64 一次，使用 `--chat-agent` |
| 跨设备协作、换负责人与附件验收 | Linux x64 一次，使用 `--phase4` |
| 公共节点、排队与私有 relay 令牌验收 | Linux x64 一次，使用 `--public-node` |
| 全网入网、志愿 relay、relay 选择与无网页 Node 验收 | Linux x64 一次，使用 `--mesh` |
| 写入保护、状态与诊断包验收 | macOS ARM64 一次，使用 `--phase5` |
| Hourly npm 包（扩展运行且声明版本为 Hourly 时） | 组装四个原生包，四个平台分别安装并检查版本；macOS ARM64、Linux x64/ARM64 另执行 `--web-only` 和 `--npm-service` 原生服务验收 |

`--npm-service` 仅允许无既有 Vibeface 数据或默认服务的 GitHub hosted runner，使用真实 launchd 或 systemd user manager，不能在开发者机器运行。Linux job 显式准备 user manager 和 D-Bus；不可用即失败。验收通过公开 npm CLI 执行安装、启动、停止、重启和卸载，核对服务使用 scoped npm vendor 中的原生程序。

`npm-package` 每次只读取一次公开 `hourly` dist-tag，固定版本、registry integrity、完整产品 manifest 及全部 vendor 字节，使用共享发布来源门证明基线业务来源是候选源码的祖先。基线单独上传，三个平台按不可变 artifact ID 下载同一份，不混入候选发布 artifact。基线与候选是同一个源码提交、基线业务来源不是候选的祖先，或 data epoch 不兼容均失败；基线业务来源等于候选提交是允许的，记为 `business_source_relation=same`。

受控 Node 和 App Server 夹具从不可变的公开基线包创建真实非空消息及附件，安装真实服务并核对运行状态与数据，再停服，以独立 prefix 核验的本 CI 候选包替换原 prefix。首个正式版前 `DATA_EPOCH` 固定为 1，同 epoch 不代表 schema 兼容。若候选明确返回 `data_schema_mismatch`，必须核对两个已审源码的 schema 指纹不同、实际启动同样被拒绝，并证明基线身份、配置、数据库及附件字节未变；保留旧目录，再为候选初始化新目录及非空消息和附件。其余错误仍由正常启动验收判定，不回退为新目录。

两包都验证活动 Node、updater 锁、pending journal 和第二 updater 的无修改拒绝。完整的安装、状态、停止、启动、重启换 PID 和卸载生命周期由最终候选包验收；重启后核对其初始身份、配置、签名历史和附件字节，并确认已完成的受控调用未重放，卸载保留数据。schema 兼容时这些数据来自原公开基线，否则来自候选新目录，报告明确区分。公开基线不承担完整生命周期验收。收据分别记录两包的真实 version、source、摘要和来源关系；候选声明版本相同或较低时如实标注，不称为 SemVer 升级。不做跨格式迁移，也不据机械版本变化宣称业务升级或真实 Provider 验收。

快、慢测试组均使用 `opt-level=1`，保留调试断言和整数溢出检查，调试信息使用 `line-tables-only` 保留回溯文件和行号，共享独立的 `test-opt1` Rust 缓存；Clippy 使用单独的 `check` 缓存。两组运行前校验每个慢测试名恰好匹配一个已有测试。慢组用 `--include-ignored` 运行列出的用例，其他 `ignored` 用例仍默认忽略。两组测试命令用 `/usr/bin/time -v` 记录 CPU 与 maxRSS；maxRSS 不能当作并发进程树的总峰值。

Rust 测试额外记录 `compile`、`enumerate` 和 `execute` 三阶段。先以同一包和测试配置运行 `cargo test --no-run`，再枚举并验证慢测试分组，最后执行原有快、慢测试命令。执行阶段仍包含 Cargo 校验和文档测试准备，不能当作精确的纯测试 CPU 时间。`--timings-file` 将阶段时长和退出码写为 JSON；CI 上传该文件并写入 job summary。编译失败或测试失败继续使 job 失败。

Linux 组包 cache 另外覆盖独立 probe workspace 的 `.artifacts/delivery-probe/target`，保留各自 Cargo.lock 的失效规则。Rust/App 缓存键包含实际使用的 `release-rustflags.py`。缓存命中只复用编译输入与依赖，不跳过类型、签名、真实进程或 UI 验收。

CI 不使用真实 AI Provider 或模型密钥。各模式都使用受控 SDK 和 App Server 夹具，`--phase5` 不启动 App Server。模拟协议通过不能证明真实模型理解需求或正确选择工具。

产品 `v<version>` 与官网 `www-v<version>` 分别进入发布路径，必须匹配声明版本，并通过同一共享来源门验证 main 源码或受信机械 Hourly 提交。产品并行构建 Web、四个原生目标、Android 和 iOS，各平台再按自身输入组包并构建桌面 App。每组校验后立即向同一个公开 Release 追加附件，完整审计完成后才追加全局 `release.json`。`publish=false` 手动入口只验证并生成制品。完整 Release 发布后，对应发布工作流按 Tag 自动派发产品或官网部署；独立部署命令仍可显式选择已发布的 Stable、RC 或 Hourly Tag；派发成功不代表部署成功。详见 [构建与发布](releasing.md)。

## 本地按改动补充验收

CLI 客户端改动先运行 `python3 scripts/check-cli-parity.py --self-test` 和 `python3 scripts/check-cli-parity.py`。覆盖检查对比 Web 实际业务调用、原生客户端类型与具名命令目录，不替代实际行为验收。当前提交的 CLI/Core 构建后，执行：

```sh
python3 scripts/check-cli-parity.py --binary /absolute/path/vibeface --output .artifacts/cli-parity.json
python3 scripts/verify-cli-client.py --binary /absolute/path/vibeface --core-binary /absolute/path/vibeface-core --artifacts .artifacts/cli-client
```

CLI 进程验收仅使用所属 Node、合成身份与受控 Provider，验证签名聊天、稳定请求 ID、附件、并行订阅、设置、问题、停止及丢失回执核对。它不调用真实模型，不启动用户后台服务。Core CI 的 Linux 制品验收也运行这两个检查。

开发者修改同步、恢复、调度或工具执行时，除 PR 核心检查外，必须执行对应专项并记录命令及结果；不能只依赖下次定期运行。每次只跑相关模式。安装器或平台交付改动应额外手动触发完整 Core 或对应平台入口，确认目标平台制品。

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
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-public-node --public-node
python3 scripts/verify-node.py --binary "$binary" --core-binary "$core" --crypto-module "$crypto" --artifacts .artifacts/local-mesh --mesh
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

Node 之间的传输、同步、地址簿、跨入口转发和投递改动跑默认模式（不带模式参数）。它启动四台真实 Node 和一台其他网络的 Node：P 运行不开放的成员 relay，B 只绑定 IPv6 loopback，其他 Node 只绑定 IPv4 loopback，协议族隔离使 B 必须经 relay 连接；B 不入全网（`--no-mesh`），凭 P 所发邀请的令牌经这台 relay 加入。A 负责调度。覆盖经 iroh 加入与同步、签名入口记录（浏览器 WASM 验签，伪造记录被拒）、四条跨入口转发路径与非成员拒绝、成员 Gateway 的 Origin 放行、调度 Node 离线时入口保留并投递且只执行一次、经另一入口重发得到原结果、崩溃与重启后不重放、relay 中断时直连继续而只经 relay 的 Node 在 relay 恢复后追平、入口切换后每条消息只出现一次，以及撤销后不再列出和转发。

队列、停止回复、待核对和事件日志改动跑 `--phase1`；工作派发、App Server 直连、请求处理、分诊、决定回传、中断、托管与崩溃恢复改动跑 `--chat-agent`。`--phase1` 包含跨 Node 的父子任务依赖与收敛；`--chat-agent` 使用单 Node。`--chat-agent` 的夹具 App Server 启动时核对 `crates/bridge/schema/app-server-methods.json`，缺一个客户端请求即失败；夹具 Agent 用真实 CLI（`vibeface tool dispatch_work`、`vibeface codex call|send|watch`、`vibeface requests|respond`、`vibeface triage events|raise|dismiss`）工作。覆盖直接派发与重试只开一轮、Bot `codex send` 作为工作 steer 进行中的回合（同一 `--id` 只回读、换内容被拒、Bot 轮外拒绝，两项工作共用一个回合并各自回报一次）、未知结果按 clientId 对账不重发、服务端请求由 Agent 应答一次、置之不理时回退拒绝、无 Agent 时立即回退、hook 经分诊在拥有会话的聊天里提问并发出 HMAC 签名 webhook（重试同一 event_id、4xx 进死信、回答后再发 `answered`）、挂住请求的提问经 `answer` 保存本人签名答案后，由持有原请求的 Node 领取独立的私有回答控制，核验原请求、传输代次、权限及响应 schema，只向仍活跃的原请求应答一次（该控制不进入 Bot 的 Main/Task 回合，结果未知时不重放）、文字回答经 Bot `codex send --id a-<问题>` 作为一项工作只送达一次、外来会话不提问、由运行它的设备归给主 Bot 一次（重复 hook 只写一次，事件照常进分诊；Bot 轮次列出名下会话，Bot 用 `hand_over_session` 转给其他 Bot 与不再关注各写一次、重试只读回、过期版本被拒、转走后原 Bot 不能再改，不再关注后静默，换主 Bot 后下一个会话归新主 Bot，之后的提问进接手的 Bot 聊天；`codex exec` 等非交互会话不归属也不分诊），以及 decision token 经 HTTP 回答拥有会话的提问一次、回答经 Bot 作为一项工作送达会话一次（不向已失效的会话发送）。

跨设备派发与停止、负责人切换、撤销后结果待确认、待核对结清、跨入口加入和附件上传改动跑 `--phase4`。它启动四台 Node：A、B、C 都带夹具协调 Agent 和 App Server，P 是运行 relay 的公网角色。跨设备工作是 Agent 交给 Agent：目标设备的 Agent 自己开 Codex 线程；目标上的服务端请求变成拥有该线程的聊天里的提问，在 A 回答后由 A 的 Bot 经 `respond --node` 转发应答一次。C 接任务后离线并被撤销。

公共节点、relay 准入和浏览器转发排队的改动跑 `--public-node`。P 是不入个人网络的公共节点，在随机网络名的全网里当 bootstrap 节点，提供显式开放并设限的 relay 和面向任何设备的浏览器转发，转发名额和队列各为 1；M 只把 P 配成 bootstrap 节点。覆盖浏览器经 P 加入 M 并对话、P 重启后重连、decision token 经 P 的 `POST /api/decision` 以 `vibeface/decision/1` 转给 M（P 不核对成员、不验 token）并回答 M 上的提问一次、回答经 M 的 Bot 送达线程一次且错误 token 得 401、名额占满时第二个浏览器排队并在轮到后接通、带运营者令牌的私有 relay R 在 A 与只有 IPv6 的 X 之间转发且 X 换成错误令牌后不可达，只开 TLS 的 Node 用 `init --tls` 的证书应答钉住它的客户端、换一张证书校验失败、明文 HTTP 得不到应答、数据目录换成另一对证书和私钥后不重启即用新证书应答，以及 P 不存事件、会话、日志和附件。

全网、志愿 relay、relay 选择或无网页 Node 改动另跑 `--mesh`，全程用 CLI 和 RPC 驱动，不开浏览器。四台 Node 用一个随机网络名并打开 `mesh.allow_private`：A 是 bootstrap 节点并打开志愿 relay，自己不存业务记录；B、C 入网后都持有 A、B、C 的在线记录，先直连同步，C 改为只用 `[::1]` 重启后选 A 的 relay 做 home 并经它与 B 同步业务数据；D 只把 B 配成 bootstrap 节点，入网后拿到全部四条记录；D 停止时用 `mesh bootstrap remove` 撤掉 bootstrap 配置，B 也停止，D 重启后凭旧表重新入网；A 关闭志愿 relay 后，D 看到 A 撤回 relay 角色，B、C、D 都不再用它做 home。无网页 Node 覆盖 `serve --no-web`、单独的 `--no-web` 和持久化的 `web_enabled=false`：没有公网 TCP 监听、本地路径 404、真实 hook 照常工作、工具鉴权 401、同步继续，入口撤回且 `gateway_url` 为空。真实公网 NAT、公网 HTTPS 网关、浏览器渲染与真实 Agent 工具执行另行验收。

写入闸门、对齐、分叉、日志、`status` 与诊断导出改动跑 `--phase5`。A、B 两台 Node：A 从备份恢复后对齐前拒绝写入、对齐后无分叉；B 离线时 A 只读，用户确认后才写；复制的数据目录在 B 处分叉后只有副本停写并列出冲突，原 A 与 B 继续写；同时检查 `status` 的对端连通与上次同步、`node.log` 权限与内容、诊断包和日志里没有能力、口令或邀请。

CI 不运行界面用例；页面验收、其他浏览器渲染、真正模型调用、实体设备授权、实际公网 NAT 与用户后台服务安装需单独按任务验收，不属于 CI 的通过声明。

Rust 快、慢组在计划生成后直接开始，Clippy 另等待静态快检。它们不等待或下载 Web 构建产物。制品任务继续等待 Web 和密码学资源，并执行原有的源码身份、真实进程和浏览器验收。

## 缓存与并发

Core 的 concurrency group 包含调用方 workflow、事件、Git ref 和 package policy。标准 Core 的首次 main push 或手动验证会取消同组旧运行，main 累计覆盖选择仍保留未验证的改动；重跑仅在 `run_attempt > 1` 时追加 `run_id`，不取消新提交的运行。Core 的 schedule 和 merge queue 不取消同组正在执行的检查。

Hourly 协调器、产品发布与部署分别遵守自己的并发规则；Hourly 协调器使用全局 `hourly-release` 串行组。

Linux x64 制品任务使用 pnpm 缓存。

`ci` 工作流中的 Rust 缓存仅在 `refs/heads/main` 或显式 `workflow_dispatch` 时保存。手动运行可为相同 ref 的后续运行预热。新增 profile 或缓存 key 在首次保存前需要冷构建。产品或官网 Tag 使用独立发布工作流，不属于本节缓存保存规则。

Rust 缓存命中不表示当前 workspace 无需编译。每次仍由 Cargo 核对源码和构建参数；评估缓存时同时记录编译、保存与还原的耗时。

当前 pinned `rust-cache` 主要保存第三方依赖，未启用 `cache-workspace-crates`。单独打开该选项仍不能保证新 checkout 复用本项目产物：Cargo 会因源码文件时间更新而重编，而且精确命中的不可变缓存不会追加保存。这里保留现有配置，不通过改写源码时间绕过 Cargo 的失效检查。

Android 使用官方 Gradle 缓存动作和 Gradle build cache，保留 `arm64-v8a` 与 `x86_64` 的 release 构建。手动运行可以写入其 Git ref 的缓存。缓存命中仍需执行原构建和制品检查，不能替代本次提交的验收。

## 合并门禁

`ci-gate` 要求选中的任务成功，选中任务失败、取消、缺失或跳过均拒绝通过。未选中的任务允许跳过或缺失；若它们实际报告失败或取消，仍拒绝通过。gate 核对冻结提交范围并重新计算计划，拒绝缩小 `scopes`、必需任务或其他选取结果。它服务于 main、merge queue、手动运行与复用调用，不是开发分支 PR 的必需检查。

Core 的 plan 与 gate 都从同一个内部 `package_policy` 重新计算覆盖范围，拒绝将标准四平台计划缩成 Linux-only 或关闭 npm。未选的 npm 与非 Linux 包不计为已验证。

GitHub 主分支规则不要求 PR 先更新到最新 base，也不将 `ci-gate` 列为必需状态。仓库开发流程仍要求相关本地验证、最终 HEAD 的 preflight 和明确合并授权。合并后的 CI 与产品或官网 Tag 的制品、发布和部署检查分别记录。

产品与官网 Tag 分别由 `release.yml` 和 `release-www.yml` 校验和发布；main 的普通 CI 不公开产品。参见 [按产品组合发布](releasing.md#按产品组合发布)。


## 桌面专项验收

桌面四平台窗口与 Windows native 检查不在 Actions 中运行；Linux 上的原生 GPUI 测试与 Clippy 由 `CI / Core` 的 `desktop-check` 执行。

本地桌面窗口测试通过现有 e2e runner 的 `tests/e2e/e2e.native-desktop.config.ts` 注册为 tools-only 原生目标，helper 调用 `scripts/e2e-desktop.mjs` 启动打包后的原生程序。独立 profile 与显式受控夹具模式隔离真实数据，按 PID 定位真实 OS 窗口，发送原生键盘输入与 Enter，保存窗口 PNG，再核对 GPUI 输入、焦点与动作回执、正常退出和没有自动启动 Node。Cargo 原生功能测试覆盖业务状态与本机生命周期；窗口驱动不宣称完成 IME、无障碍或完整业务矩阵验收。Windows 另核对真实 SID/DACL、维护通道、Unicode 路径、Provider 子进程和当前用户服务。所有 Provider 均为受控夹具，不调用真实模型。

离线文档使用 `scripts/offline-docs.json` 作为唯一清单；`offline-docs.py` 由 compose、POSIX 和 Windows 组包入口共同调用，复制所有已跟踪公开文档并改写相对链接。测试检查最终组装目录中的本地链接闭包，源码引用固定到本次完整提交。

## 会话分页的交付检查

修改会话分页时，至少核对 `crates/protocol/src/history.rs` 的 19 分区与所有客户端解析器一致，并执行 Core 的 `conversation_history`、`conversation_sections`、`conversation_settings`、`scheduler_history` 专项，原生 `native_ui_history` 和共享 Client 的 history 测试。需要覆盖空页继续游标、字节预算、稳定身份去重、同一连接刷新时保留已加载旧页、重连或连接代次变化后清空旧投影、完整替换后的删除、治理/分叉/owner 失效，以及网络或会话切换后的晚到响应。源码有测试不等于该 HEAD 已执行通过。
