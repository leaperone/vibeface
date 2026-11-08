# 运行端到端测试

e2e 使用 Chromium 打开真实 Node 内嵌的 Web 页面。需要 Node.js 24.8+，或 22.22.3+，以及 pnpm 11.9.0。

```sh
pnpm install --frozen-lockfile --filter .
pnpm test:e2e:check
pnpm test:e2e
```

默认从 PATH 运行 `vibeface`。测试源码改动时，使用对应提交构建的完整程序：

```sh
VIBEFACE_E2E_BINARY=/absolute/path/to/vibeface \
VIBEFACE_E2E_APP_DIR=/absolute/path/to/apps/app/dist/client \
pnpm test:e2e
```

程序的构建步骤见[源码构建](releasing.md#ci-验证范围)。本地默认把 Chromium 下载到 `tests/e2e/.artifacts/browsers/`。Linux CI 先执行以下命令：

```sh
PLAYWRIGHT_BROWSERS_PATH="$PWD/.artifacts/e2e/browsers" pnpm exec e2e-web install chromium --with-deps
```

每项测试创建独立 Node 数据目录，使用随机本机端口，关闭 Codex 会话扫描。默认 fixture 关闭模型执行；`workspaceTest` 启用本次测试的受控 Codex 程序，不连接真实模型。测试结束后停止自己的进程，不连接已有 Node。测试控制口只监听 loopback，认证材料保存在 owner-only 文件。Node 数据和控制文件位于 `.artifacts/e2e/`，runner 日志、报告和下载文件位于 `tests/e2e/.artifacts/`，都不会提交。恢复密码每次随机生成，通过 e2e secrets 传入。

示例检查建网页面。核心测试依次建网、下载恢复文件、跳过 AI 设置、创建 Bot，再刷新确认它仍存在。传输测试启动发送 Node、接收 Node 和公共入口 Node，经真实界面创建两个独立个人网络，从“设置 → 实验室”开启文件传输，再到 `/settings/lab/transfers` 确认零费用订单，接受订单并重启接收 Node。它还关闭实验并刷新，检查记录仍在、已收到文件仍可保存、重开不新增订单且不改变原传输方式。浏览器下载的原文件必须通过 SHA-256 核对，免费过程不创建钱包或调用链上 RPC。这些测试不需要模型或登录。CI 使用本次提交打包出的 Linux 程序运行。

headless 传输测试仅阻止临时文件输入打开原生系统选择框，再用真实 `setInputFiles` 选择任务内文件。文件输入的 change 事件、原文件内容、产品加密和 Node 请求均保留真实路径。下载结果仍需独立核对 SHA-256。

测试使用 `tests/e2e/fixtures.ts` 导出的 `test`、`deliveryTest` 或 `workspaceTest`，每项测试均重建 Node。保持 `workers: 1` 和 `retries: 0`，避免重复执行有外部副作用的验收。

`workspace.e2e.ts` 从真实表单创建 Bot、独立 Task 和频道，检查连续补充、稍后队列控制、Context 来源、scope 隔离及窄屏输入和停止。控制器只释放固定的受控 Provider 门，并读取实际原生输入和回合记录，不直接写业务表。390px 网页通过不能替代原生手机制品验收。

公共发现使用同一文件导出的 `bootstrapTest` 和 `httpBootstrapTest`。每项测试运行独立 Discovery、公共入口和新节点，不配置运营者口令。网页自身的连接接口返回 503，浏览器必须验证目录签名并使用目录里的另一处入口，完成真实安全码配对后读取助手列表。HTTP 专项还会把节点的 UDP 发现地址设为不可达，确认节点通过 HTTPS 获取接入许可。普通夹具关闭公共发现，并阻止请求默认公共服务。

```sh
pnpm exec e2e run --config tests/e2e/e2e.config.ts bootstrap.e2e.ts
```

只运行核心测试或显示浏览器：

```sh
pnpm exec e2e run --config tests/e2e/e2e.config.ts onboarding.e2e.ts
pnpm exec e2e run --config tests/e2e/e2e.config.ts --headed
```

排查失败时，读取终端指向的 `trace.md`。不要上传 Node 数据目录或恢复文件。

## 验证公益传输进程和网络边界

先构建仅用于受控 loopback 验收的 QUIC/Noise probe。构建产物必须留在任务 worktree 的 `.artifacts/`。

```sh
VIBEFACE_PROBE_SOURCE_ROOT="$PWD" \
CARGO_TARGET_DIR="$PWD/.artifacts/delivery-probe/target" \
cargo build --manifest-path tests/fixtures/delivery-probe/Cargo.toml --locked

python3 scripts/verify-node.py --delivery-charity \
  --binary /absolute/path/to/vibeface \
  --core-binary /absolute/path/to/vibeface-core \
  --artifacts .artifacts/delivery \
  --delivery-probe .artifacts/delivery-probe/target/debug/vibeface-delivery-probe
```

该模式运行四个全新 Node，验证免费签名目录、默认关闭、两窗公益传输、同 intent 幂等、明确接收同意和原文件恢复。probe 使用真实产品发现代码和受控身份，正例必须成功到达指定 PeerID 的服务，外部 Node 必须在真实私网权限检查处遭到拒绝。验收先上传 8192 字节真实密文，再强制停止公共入口 Node，重启后检查持久前缀和旧 offset 拒绝，最后由产品继续完成订单。摘要保存在输出路径的 `summary.json`。

构建与执行目录不同时，分别传入 `--crypto-module`、`--delivery-profile` 和 `--delivery-source-root` 的绝对路径。运行时会核对 probe 内嵌发现代码的 SHA-256，拒绝使用其他源码的旧 probe。

## 验证受保护付费交付

当前 Node 只提供 Recoverable 预算付款。旧 `verify-node.py --delivery`、Strict/十三证明 `run-delivery-live.py` 和旧 JSONL mutation 控制器已明确拒绝原启动方式；不能用其旧命令或旧截图当作当前产品验收。默认公益进程验收沿用上节，不产生链或钱包副作用。

受保护真实场景使用正常当前时钟、固定产品 profile、Linux cgroup v2 和真正的 S/P/R 进程。R 先注册新的 genuine KeyBinding；S 开一个精确预付预算，分别 prepare 两笔新的 paid/recoverable 单窗口订单，再经管理成员 `confirm_batch` 明确授权同批。后台按原证明锁产生两个 genuine Source，一笔 reserveBatch 分配到不同订单 slot。R 经 `accept_batch` 明确同批接收并串行产生两个 genuine Hold；P 的正式预算状态 `ready_windows` 同时出现两窗后，操作者在既有实验室页或 `wallet budget flush` 明确同批核验/揭钥。正常恢复不再自动要求 Open。

最小样本为每单 delivery=5、reservation=bonus=0、预算10：一份 KeyBinding 加两份 Source、两份 Hold，共五份不同真实证明。必须核对真实 reserve/hold/reveal 三种方法各自含2slot、逐单 earned5、预算 withdrawn10，以及固定 P 钱包实际净增10。S 在传输必要对象后停止，不提供任何事后签名；R 读取并校验原文件、名称、字节和 SHA-256。样本不能把旧17证明改成5，也不能把一份证明充作第二个订单。

公开 schema2 bundle 只包含已披露 capsule、公开证明、交易哈希和固定条款。官方 host 对每份 binary receipt 独立验真并拒绝错 journal；`observe-settlement.mjs` 只读同一原链的真实 runtime、canonical 条款/预算/窗口、逐窗 Source/Hold、批次 calldata、事件、余额与 gas。固定源码、CLI/Core 制品摘要、真实进程重启、钱包 lock/off 门禁、披露恢复故障和实际渲染分别留证，任何一层未执行均标未验证。

本轮 proof 构建和新genuine驱动不得放宽原7200秒证明硬界、现有 deadline、11GiB/zero-swap/64PIDs、image/profile 或时钟。界面验收按本文件 e2e 规则执行最小真实表面；源码、类型检查、HTTP200 或合约测试都不代替浏览器/设备证据。操作和安全模型见 [受保护预算](paid-delivery.md)；链上观察格式见 `contracts/delivery/README.md`。

宿主和手动 CI 使用同一个正式入口。先运行纯自检，再提供当前 clean HEAD 的 Linux package 下载目录或已解包目录：

```sh
python3 scripts/run-delivery-live.py --protected-selfcheck

python3 scripts/run-delivery-live.py \
  --protected-package "$PWD/.artifacts/exact-head-linux-package" \
  --source-commit "$(git rev-parse HEAD)"
```

该入口复用 `check-delivery.py` 的 canonical sandbox 构建配方，固定 Linux amd64、11GiB、zero swap、64 PIDs 和两个 Tokio workers。源码与任务缓存仍在本 worktree 的 `.artifacts/`；linked worktree 的 common Git metadata 和 `.git` 只读挂载，显式设置 `GIT_DIR`、`GIT_WORK_TREE` 和 `GIT_OPTIONAL_LOCKS=0`，始终核对 clean exact HEAD。容器以宿主 UID/GID 运行，配置在容器内以 `0600` 创建；不会改变既有缓存权限或复制工具链。

本机入口按原生驱动既有阶段和业务期限等待，不再套用旧 Strict/UI 的 20700 秒整轮截止。KeyBinding 观察上限仍为 7400 秒，Source、Hold 各为 16000 秒；每次证明的 7200 秒硬界、原 UUID、报价/持有/密钥截止、费用与 W/I/F/S 均不变。每份完成的证明仍立即独立验真并保存。同一个运行容器可以跨宿主工具轮次继续等待；停止容器后不支持恢复原链/订单，不能把已保存分项称为已支持跨容器续跑。

`prepare`、`confirm_batch`、`accept_batch` 只有在结构化响应明确返回提交前的固定错误 `another delivery operation is in progress` 时，才沿既有 120 秒阶段上限等待重试，原 UUID 和参数不变。超时、网络异常、未知结果及其他拒绝直接失败，不自动重发。

GitHub Actions job 的物理上限仍为 360 分钟，这是独立的外部限制，可能先于原生阶段完成而终止 job，不代表单项失败或完整实验通过。需要继续同一合法原生运行时，应保留本机的原容器、原链和原业务期限；不得靠新 UUID、延长期限或重部署链绕过原边界。

容器核对 package checksum、`BUILD-INFO.json`、当前 JS/WASM 和 build-input receipts，先以真实 Web 资源进程检查实测 package 的 libc。仅当日志实际证明 GLIBC 不兼容时，按当前 clean HEAD 和现有 release flags 重建 CLI/Core，并再次检查真实资源进程。package 的检查状态、实际使用的 binary 来源及摘要分别写入 `artifact-provenance.json`，不能用 bare binary hash 自证源码来源。随后调用固定 guest 安装器和官方 `build.sh`，核对 image/profile/ELF，生成配置并调用现有 `verify-node.py --delivery-protected`。Web 资源进程检查不代表渲染验收。

输出位于 `.artifacts/vdp/protected-runs/<run-id>/`；可用 `--protected-run-dir` 指定另一个全新任务目录。只上传 launcher 输出或 `GITHUB_OUTPUT` 的 `public-directory` 所指安全目录，实际路径为本轮 run 的 `public-export-<opaque>/`。其中包括逐项认证且当前摘要匹配的 receipts/metadata、原链公开观察、制品来源、资源状态和最终摘要。导出的 `progress.json` 只保留受控状态与合格清单，完整原生终态结果由摘要认证的 `native-result.json` 保留。

未登记、临时、私密或损坏材料不导出，也不妨碍其他已验真分项导出；整体仍为 FAIL，并写出安全摘要和 `public-directory`。任何 Node、证明、清理、资源或制品检查失败都不能标记整体 PASS。配置、钱包、原始文件和日志留在私有 run 内，不能上传整个 run 或 `native/public/`。

已准备好受限 sandbox 和全部精确制品时，也可以使用不构建、不下载或启动 Docker 的低层 `--protected-config`。配置 schema 为 `scripts/verify-delivery-protected.py` 的 `validate_config`；在本任务 `.artifacts/` 写入 `0600` 配置，所有摘要必须填实际值，不能沿用其他源码或旧 receipt：

```json
{
  "schema_version": 1,
  "source_commit": "<final 40-character commit>",
  "paths": {
    "repository": "/absolute/current/task/worktree",
    "binary": "/absolute/artifacts/vibeface",
    "core_binary": "/absolute/artifacts/vibeface-core",
    "crypto_module": "/absolute/artifacts/vibeface_secure.js",
    "prover": "/absolute/artifacts/vibeface-delivery-prover",
    "r0vm": "/absolute/artifacts/r0vm"
  },
  "sha256": {
    "binary": "<actual sha256>", "core_binary": "<actual sha256>",
    "crypto_module": "<actual sha256>", "crypto_wasm": "<actual sha256>",
    "prover": "<actual sha256>", "r0vm": "<actual sha256>", "profile": "<actual sha256>"
  },
  "elf_sha256": "<actual guest ELF sha256>",
  "caps": {
    "deposit": "10", "max_service_amount": "5",
    "source_gas_cap_wei": "100000000000000000",
    "provider_gas_cap_wei": "100000000000000000",
    "register_key_gas_cap_wei": "100000000000000000",
    "native_funding_wei_each": "10000000000000000000"
  },
  "publication_policy": {
    "watchdog_seconds": 60, "inclusion_seconds": 60,
    "finality_seconds": 10, "clock_skew_seconds": 5,
    "max_fee_per_gas": "3000000000", "priority_fee_per_gas": "1000000000"
  }
}
```

```sh
python3 scripts/run-delivery-live.py \
  --protected-config "$PWD/.artifacts/protected/config.json" \
  --protected-run-dir "$PWD/.artifacts/protected/run"
```

每次要求全新输出目录，动态部署原链并创建全新钱包、注册和两笔订单；不复用原钱包、witness 或 proof。也可使用 `verify-node.py --delivery-protected`，显式传匹配的 `--binary`、`--core-binary`、`--crypto-module`、`--artifacts`、`--delivery-config` 和可选 `--delivery-run-dir`。低层驱动在其 run 的 `public/` 保留逐份验真的五份 receipt、schema2 bundle、原链观察和 `progress.json`，但它本身不核验 bare binary 的构建来源；调用方必须先完成上文的制品来源检查。

该运行只证明新的 4096 字节双单场景。它核对 S 离线后 nonce 不增、P 自动累计提款、R 实际读取，以及 S/R/P 真正重启、locked/off 后拒写与历史读取；不覆盖 UI 渲染、旧 fee5 同字节或最大容量、强制 reorg、nonce-consuming revert、临近截止和异常代币矩阵。这些仍需独立证据。

## 使用 Device Hub 验收本机 Apple 设备

Device Hub 是 Xcode 自带的 Apple 设备管理应用，位于 Xcode 应用包的 `Contents/Applications/DeviceHub.app`。它用于查看和管理本机的真机与模拟器；自动验收仍由 e2e 驱动 App 并检查结果。simslim 用于精简模拟器后台服务，不负责判断产品验收是否通过。

1. 在 Device Hub 中确认目标是真机还是模拟器，并记录型号、系统版本与 UDID。可用 `xcrun devicectl list devices` 交叉核对。设备名称可能重复，安装和操作必须绑定 UDID。
2. 模拟器验收使用当前任务独立创建的设备。将其 UUID 传给 `VIBEFACE_E2E_SIMULATOR`，按下节运行原生 e2e。该入口会重新安装 App 并清除专属模拟器的钥匙串，不要使用日常设备或其他任务的模拟器。
3. 真机验收使用明确指定的设备，并先检查配对、开发者模式和签名条件。真机需要签名有效的 `iphoneos` 构建，Simulator `.app` 不能安装到 iPhone。覆盖安装时保留原有身份和数据，不卸载、重置或清除钥匙串。
4. 安装后实际启动 App，按本次改动验证界面和操作。在任务 planning 的证据目录记录源码提交、构建来源、目标设备和验收结果。设备可用、安装成功和 App 启动成功不能代替功能验收通过。

本地 iPhone 真机、专属模拟器和 CI 创建的模拟器分别记录验收结果。Device Hub 不上传 TestFlight、不发布 App Store，也不替代签名或发布流程。CI 的模拟器创建与 e2e 仍按 workflow 执行，不依赖打开本机 Device Hub。

## 验证原生接收和本地文件

原生验收使用已有 e2e mobile 引擎，必须指定当前任务构建的 `.app`、Node、匹配 WASM，以及专用模拟器的 UUID。该入口会重新安装本任务应用并清除该专用模拟器的钥匙串。不要指定其他任务正在使用的模拟器。

先在任务 worktree 根目录构建模拟器 `.app`，命令与 `.github/workflows/mobile.yml` 一致，只把产物放进 `.artifacts/native/`：

```sh
scripts/build-mobile-secure.sh ios
(cd apps/mobile && npx expo prebuild --platform ios && \
  xcodebuild -workspace ios/Vibeface.xcworkspace -scheme Vibeface -configuration Release \
    -sdk iphonesimulator -destination 'generic/platform=iOS Simulator' ARCHS=arm64 \
    -derivedDataPath ../../.artifacts/native/ios-build \
    CODE_SIGN_IDENTITY=- CODE_SIGN_STYLE=Manual DEVELOPMENT_TEAM= build)
cp -R .artifacts/native/ios-build/Build/Products/Release-iphonesimulator/Vibeface.app .artifacts/native/
```

必须带 `CODE_SIGN_IDENTITY=-`：ad-hoc 签名会把模拟器 entitlements 写进二进制，缺少它们时钥匙串会拒绝 SecureStore。

```sh
VIBEFACE_E2E_IOS_APP=/absolute/task/.artifacts/native/Vibeface.app \
VIBEFACE_E2E_SIMULATOR="$TASK_SIMULATOR_UUID" \
VIBEFACE_E2E_BINARY=/absolute/task/.artifacts/native/vibeface \
VIBEFACE_E2E_CRYPTO_MODULE=/absolute/task/.artifacts/native/web/vibeface_secure.js \
AGENT_DEVICE_HOME="$PWD/.artifacts/native/agent-device" \
AGENT_DEVICE_STATE_DIR="$PWD/.artifacts/native/agent-device" \
pnpm exec e2e run --config tests/e2e/e2e.native.config.ts --grep '^receives a charity file'
```

原生测试通过真实邀请码加入接收网络，检查手机与管理员的安全码相同，再授予管理权限。测试从私有任务文件读取一次性邀请码，像扫码一样以 `vibeface://join/<票据>` 打开应用。发送方经真实 Node 发出公益订单，原生界面接受并保存文件。测试只读该专用应用的 `Documents/transfers/`，以独立 SHA-256 核对实际保存的字节。没有安装新构建、执行原生界面或读取实际文件时，不能把入口准备完成或 Web 通过作为原生验收成功。

## 验证原生工作台

`workspace.native.ts` 使用同一任务构建的 iOS App 和受控 Node。先新建名称以 `vibeface-workspace-` 开头的专属模拟器并启动，把 UUID 写入 `.artifacts/native/workspace/simulator.txt`。不得复用其他任务的模拟器。邀请文件保持在同一目录，权限为 `0600`。

```sh
VIBEFACE_E2E_IOS_APP=/absolute/task/.artifacts/native/Vibeface.app \
VIBEFACE_E2E_SIMULATOR="$TASK_SIMULATOR_UUID" \
VIBEFACE_E2E_SOURCE_COMMIT="$TASK_SOURCE_COMMIT" \
VIBEFACE_E2E_BINARY=/absolute/task/target/debug/vibeface \
VIBEFACE_E2E_CRYPTO_MODULE=/absolute/task/apps/app/dist/client/vibeface_secure.js \
pnpm exec e2e run --config tests/e2e/e2e.workspace.native.config.ts workspace.native.ts:123 --retries 0 --video off
```

用例核对已安装 App 与构建文件，通过邀请、安全码和管理授权加入，再从原生界面创建 Bot。主聊天检查历史阅读、回到底部、键盘、新回复保留阅读位置及聊天切换，随后创建 Task 和频道，检查补充、Context、队列编辑取消及停止后的草稿。`acceptance.json` 只在全部界面步骤通过后写入，`cleanup.json` 只在受控 fixture 正常退出后写入。运行结束后，操作者还需停止并删除本任务的模拟器。模拟器通过不代表真机、Android 或真实模型验收。

## 验证原生 Provider 设置和队列取消

手动运行 `.github/workflows/mobile.yml` 时，现有 iOS job 在当前 checkout 构建 Simulator App、宿主 Node、Web 和 WASM，然后运行一个 Provider 原生用例。Android job 仍只构建 APK。iOS job 核对实际 arm64 宿主与 App，选择可用 iOS runtime，创建专属 iPhone 17 simulator 并等待它启动。缺少 runtime、启动失败或 driver 不可用都会使验收失败，构建成功不能代替 UI 结果。

宿主构建前还要求 HEAD 等于本次提交，且 `git status --porcelain --untracked-files=normal` 为空。DerivedData 和 Simulator zip 位于已忽略的 `.artifacts/native/`，不会干扰这个检查。Expo prebuild 改变源码时必须先解决，不能继续把脏源码产物记为当前提交。

本地运行使用上节的四个明确产物与 simulator 环境变量，并补充当前源码身份。仅选择 Provider 用例，不运行公益文件用例。

```sh
umask 077
mkdir -p .artifacts/native/private
VIBEFACE_E2E_SOURCE_COMMIT="$(git rev-parse HEAD)" \
pnpm exec e2e run --config tests/e2e/e2e.native.config.ts --grep '^provider ' \
  --trace off --video off --retries 0 > .artifacts/native/private/ui.log 2>&1
```

该用例重新安装当前 App，逐项核对已安装 bundle 与构建 bundle 的文件摘要，包括实际可执行文件、打包 JavaScript 和原生库。它用 e2e Secret 从任务私有文件读取邀请，填写原生“加入链接”表单，再核对双方安全码并授予管理权限。邀请不会作为普通深链接参数传入 UI driver。iOS 的 TextInput 可把 placeholder 或值附在可访问名称后面，因此输入定位使用 textbox 角色和以字段名称开头、带分隔边界的正则，仍要求唯一匹配。已填写值、保存回读和私有命令剪贴板均作精确核对。

原生设备页的“Bot 与任务”进入真实 AI 设置页。fixture 只配置受控程序与连接模式，并关闭 Bot 和执行角色，真实返回 `manager=null`。用例先确认未启用的 Codex 尚未主动检查，再开启执行用途；依照 Node 的首次设置默认，这也开启 Bot，用例随后由原生界面关闭 Bot。启用和已有 manager 的 Node 启动都会主动检查，不能把这些结果称为未检查。用例在 execution-only 状态实际检查未登录提示，精确核对从界面复制的私有目录认证命令，由受限 fake CLI 完成一次合成登录，再主动检查并选择、手填和清空执行模型。之后开启 Bot，验证双角色草稿、独立保存、默认模型和 high。它还确认 Provider 控制强度时清除旧覆盖，包括带空白的手填已知模型，以及目录不可用时保留旧模型和强度。所有启用与保存均经已授权手机的正常接口执行。

队列场景通过真实 Node API 准备一条运行中消息和一条排队消息。原生 UI 取消排队后，再通过原生“停止”结束运行中的消息并释放槽位。独立读取确认运行中的消息调用过一次，被取消的消息始终没有调用，离开再进入聊天后计数保持不变。受控 fixture 不调用真实模型，也不代替手机保存、取消或停止。

native config 默认关闭 trace 和整段 video。原始日志、自动失败截图、e2e report、邀请、恢复文件、Node 数据与 driver 状态都属于私有材料，不能递归上传 `.artifacts/native/`。CI 只上传固定的 `provider.json`、`provider-probe.json`、`provider-initial.png`、`provider-selectable.png`、`provider-controlled.png`、`provider-typed.png` 和 `terminology-device-kinds.png`。`provider-probe.json` 只记录首轮检查的 `link`、`readiness`、`authentication`、`catalog` 和 `checked` 五个固定状态字段。

`provider-initial.png` 是唯一的配对前诊断图。它只在本次 CI 新建的专属 simulator 上，重新安装、清除钥匙串并启动 App 后主动拍摄，且早于首次 `secrets.get` 和邀请输入。用例核对 simulator 文件、实际 UUID、Booted 状态以及包含当前 run ID 和 attempt 的专属名称。此时 App 尚未接收邀请、登录命令或凭证，也没有旧应用数据；初始图只用于查看路由、系统弹窗和空表单。`terminology-device-kinds.png` 在配对后的“添加设备”面板记录节点与客户端分类，其他三张图在配对后的设置页保存。诊断图不代表配对或表单验收通过。导航诊断仅记录固定阶段和已知界面的布尔状态，不输出界面文本、加入链接、命令或剪贴板内容。CI 按固定键与布尔类型校验后，将该诊断并入公开的 `provider.json`，不上传原始页面树或测试日志。最终收据只有在一个目标测试真实通过、fixture 退出、专属 simulator 删除和私有状态清理后才标为 `passed`。

失败收据与 stdout 保留固定阶段、报告是否可读，以及从实际 e2e report 提取的受限错误码、固定 `engine_reason` 和 `engine_timeout_kind`、已知 API 名和两个验收文件的行号。`completed_poll_samples` 只来自唯一合法的既有 expect 采样事件；零表示没有记录完成的采样，字段缺失不补零。错误 message、stack、locator label、值和原始报告不会导出。

已有失败语义快照通过文件与元数据校验时，`failure_screen` 只记录欢迎、已填入链接、邀请输入、设备名输入、加入按钮及已知无效或失效提示是否被报告。`true` 是正向出现，`null` 是未知，不能当作控件不存在。快照采集为 best effort，可能缺失或裁剪；没有截断标记也不证明树完整，不能从语义文本恢复原生控件类型或输入读回值。该投影不新增 UI 读取，也不改变原 fill、用例或清理的成败。报告尚未生成时仍保留失败阶段，完整私有日志随后按原清理规则删除。

本地用例产生的 `ui_passed` 仅表示 UI 步骤已完成；本地操作者仍须关闭并删除本人创建的专属 simulator，并核对 fixture 清理结果。这个入口覆盖 iOS Simulator 与受控 Provider，不代表 Android UI、iPhone 真机、真实登录或真实模型已经验收。

## 验证原生聊天回到底部

上面的原生工作台用例使用现有受控 Provider 的 `main` 门建立长聊天，释放该门时收到真实 Node 写入的新回复。它在任务专属模拟器和对应源码的 App 中检查以下行为。

- 位于最新消息处时没有回到底部按钮；上滑阅读历史后，输入框上方出现按钮。
- 点击按钮后显示最新消息，按钮消失；键盘展开和收起时均成立。
- 阅读历史时新增消息不打断当前位置，仍可点击按钮回到底部。
- 切换聊天后重新判断位置；短列表不显示按钮。

## 让编码代理使用 e2e

项目已配置 Codex、Claude Code 和 Cursor 的 e2e MCP。安装依赖后重新打开可信项目会话。编写或运行测试前，读取 [e2e skill](../../.agents/skills/e2e/SKILL.md)。

当前未选择 AI 模型。需要 `agent.act` 时，先选择订阅、API 或本地模型，再按 `pnpm exec e2e guide setup` 配置 provider。ChatGPT 订阅的登录命令为 `pnpm exec e2e login openai`，完成登录后再运行模型测试。不要把凭证写入仓库；CI 保持不调用真实模型。
