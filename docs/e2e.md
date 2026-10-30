# 运行端到端测试

e2e 使用 Chromium 打开真实 Node 内嵌的 Web 页面。需要 Node.js 24.8+，或 22.22.3+，以及 pnpm 11.9.0。

```sh
pnpm install --frozen-lockfile --filter .
pnpm test:e2e:check
pnpm test:e2e
```

默认从 PATH 运行 `vibeface`。测试源码改动时，使用对应提交构建的完整程序：

```sh
VIBEFACE_E2E_BINARY=/absolute/path/to/vibeface pnpm test:e2e
```

程序的构建步骤见[源码构建](releasing.md#ci-验证范围)。首次运行会把 Chromium 下载到 `.artifacts/e2e/browsers/`。Linux CI 先执行以下命令：

```sh
PLAYWRIGHT_BROWSERS_PATH="$PWD/.artifacts/e2e/browsers" pnpm exec e2e-web install chromium --with-deps
```

每项测试创建独立 Node 数据目录，使用随机本机端口，关闭模型执行和 Codex 会话扫描。测试结束后停止自己的进程，不连接已有 Node。测试控制口只监听 loopback，认证材料保存在 owner-only 文件。数据、日志、报告和下载文件位于 `.artifacts/e2e/`，不会提交。恢复密码每次随机生成，通过 e2e secrets 传入。

示例检查建网页面。核心测试依次建网、下载恢复文件、跳过 AI 设置、创建 Bot，再刷新确认它仍存在。传输测试启动发送 Node、接收 Node 和公共入口 Node，经真实界面创建两个独立个人网络，从“设置 → 实验室”开启文件传输，再到 `/settings/lab/transfers` 确认零费用订单，接受订单并重启接收 Node。它还关闭实验并刷新，检查记录仍在、已收到文件仍可保存、重开不新增订单且不改变原传输方式。浏览器下载的原文件必须通过 SHA-256 核对，免费过程不创建钱包或调用链上 RPC。这些测试不需要模型或登录。CI 使用本次提交打包出的 Linux 程序运行。

headless 传输测试仅阻止临时文件输入打开原生系统选择框，再用真实 `setInputFiles` 选择任务内文件。文件输入的 change 事件、原文件内容、产品加密和 Node 请求均保留真实路径。下载结果仍需独立核对 SHA-256。

测试使用 `tests/fixtures.ts` 导出的 `test` 或 `deliveryTest`，每项测试均重建 Node。保持 `workers: 1` 和 `retries: 0`，避免重复执行有外部副作用的验收。

只运行核心测试或显示浏览器：

```sh
pnpm exec e2e run tests/onboarding.e2e.ts
pnpm test:e2e --headed
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
  --artifacts .artifacts/delivery \
  --delivery-probe .artifacts/delivery-probe/target/debug/vibeface-delivery-probe
```

该模式运行四个全新 Node，验证免费签名目录、默认关闭、两窗公益传输、同 intent 幂等、明确接收同意和原文件恢复。probe 使用真实产品发现代码和受控身份，正例必须成功到达指定 PeerID 的服务，外部 Node 必须在真实私网权限检查处遭到拒绝。验收先上传 8192 字节真实密文，再强制停止公共入口 Node，重启后检查持久前缀和旧 offset 拒绝，最后由产品继续完成订单。摘要保存在输出路径的 `summary.json`。

构建与执行目录不同时，分别传入 `--crypto-module`、`--delivery-profile` 和 `--delivery-source-root` 的绝对路径。运行时会核对 probe 内嵌发现代码的 SHA-256，拒绝使用其他源码的旧 probe。

## 验证付费交付和实际结算

完整付费模式必须在 Linux cgroup v2 环境中运行，使用固定产品 profile、真实 prover 和本机 31337 测试链。先安装合约依赖。

```sh
node contracts/delivery/install.mjs
```

在同一任务目录创建权限为 `0600` 的配置文件，使用以下字段。`deployment_file` 指向当前 worktree 的 `.artifacts/` 下尚不存在的新文件。runner 自行启动并清理该次独立部署，拒绝复用旧输出或旧链。`prover` 必须含绝对路径 `program`、`r0vm`、`cgroup` 和与 `proofs/delivery/profile.json` 完全相同的 `profile`。金额均为十进制字符串，gas 上限以 wei 表示。

```json
{
  "schema_version": 1,
  "deployment_file": "/absolute/task/.artifacts/delivery/deployment.json",
  "prover": {
    "program": "/absolute/path/to/vibeface-delivery-prover",
    "r0vm": "/absolute/path/to/r0vm",
    "cgroup": "/sys/fs/cgroup/task-proof",
    "profile": {}
  },
  "max_service_amount": "1000000",
  "source_gas_cap_wei": "1000000000000000000",
  "provider_gas_cap_wei": "1000000000000000000",
  "register_key_gas_cap_wei": "1000000000000000000",
  "settlement_observer": "/absolute/repo/contracts/delivery/observe-settlement.mjs"
}
```

用实际产品 profile 替换示例中的空对象，再执行以下命令。

```sh
python3 scripts/verify-node.py --delivery \
  --binary /absolute/path/to/vibeface \
  --artifacts .artifacts/delivery \
  --delivery-probe .artifacts/delivery-probe/target/debug/vibeface-delivery-probe \
  --delivery-config .artifacts/delivery/acceptance.json
```

该模式先验证公益路径，再创建独立测试钱包，通过受保护 FD 解锁。它只使用本机部署的公开测试资助账户，生成一份 key-binding 证明及两个窗口各自的 source、hold、open 证明，核对完整原文件和实际提现。独立观察器只读原链，复核七份证明、真实交易、余额和 gas。导出的 `settlement/` 只含公开证明、capsule 与交易哈希，不含 witness、私钥、密码或 raw transaction。

缺少配置、真实证明、部署或结算证据时，命令失败并报告 `UNVERIFIED` 或具体错误。公益模式通过不能代替付费验收。测试链的两次确认也不代表绝对最终性。

## 验证完整付费分支与双端恢复

`scripts/run-delivery-live.py` 监督同一临时链、sandbox 和宿主 UI。向 `verify-node.py --delivery` 追加 `--delivery-ui-control` 的专用路径后，runner 在原严格订单之后运行有限分支、真实密钥生命周期和普通单窗口订单。核心严格订单仍是 130816 字节、两窗和七份证明。

扩展验收在实际付款前捕获 C0，并在两个 source 证明完成、原 Source 的 approve `eth_estimateGas` 返回前捕获 C0Prime。该请求保持阻断，停稳节点并备份完整私有目录后，以失败结束旧请求。后续操作复用原意图和已完成的证明。C1 对应真实 fund 已确认、接收方尚未接受的状态。链快照和完整 Node 数据快照必须配对恢复，不修改单表或删除签名交易。

有限分支验证入金代币行为、退款、部分交付、截止、重组和固定收款地址。keeper 操作与真实 Source 自主退款分别记录。Source 自主退款恢复匹配的完整 C1 数据，只解锁 Source，完成后保存分支目录和实际交易，再停稳恢复。Node 专用 loopback RPC 转发器只计广播次数，不保存请求体或 raw。锁定观察期间广播必须为零。

真实 Rotate、Revoke、Register 各新增一份 key-binding 证明。Rotate 在一个旧窗口已 Held、另一个尚未 Held 的分支执行。原签名交易恢复后保留同一 Rotate 和旧密钥，最后的新 Active key 接收普通订单。普通订单是 4096 字节的 `ordinary-paid-ui.bin`，新增 source、hold、open 各一份。全任务共十三份不同证明，普通公开 bundle 有四项，其中 key-binding 与最终 Register 共用。

宿主通过 `e2e.paid.config.ts` 运行两个平台过滤的测试，使用 `workers: 2` 和 `retries: 0`。测试扩展名是 `.paid.ts`，默认 e2e 测试不会发现这些需要控制器的用例。两个测试都通过真实邀请、安全码和管理权限入网。Web 选择实际文件、查看费用并确认原订单。Android release app 接受订单，实际后台、移除本任务的接收端口 reverse，并终止原 app 连接。只有前台实际出现离线状态才记录断线。后台 Node 完成结算后，恢复同一 reverse，手机回到原订单并保存文件。root AVD 仅读取该 app 的 transfers 文件核对大小与 SHA。

宿主必须提供以下环境变量。APK、serial 和 controller 都由本轮 CI 明确指定。

```sh
VIBEFACE_DELIVERY_UI_CONTROL=/absolute/task/.artifacts/vdp/live-ui/control.json
VIBEFACE_DELIVERY_UI_TIMEOUT_MS=3600000
VIBEFACE_E2E_ANDROID_APK=/absolute/task/.artifacts/native/app-release.apk
VIBEFACE_E2E_ANDROID_SERIAL=emulator-5554
```

容器保持原 root 证明环境，通过 `VIBEFACE_DELIVERY_UI_UID` 和 `VIBEFACE_DELIVERY_UI_GID` 把专用控制目录、原文件和 `0600` 控制描述交给宿主用户。邀请码只经过认证 API，不落公开文件。Node 目录、钱包、密钥和 witness 不改变所有者或权限。

严格和普通 observer 的当次公开 JSON 分别保存在各自 bundle 目录的 `settlement-observation.json`，不读取一个可被覆盖的 latest 报告。有限分支只接受 helper ready 事件给出的本次 `publicIndex`。密钥生命周期公开文件位于 `key-lifecycle/`，完整私有数据快照与分支 raw 永不上传。

普通双端操作保留三个配对后的视频片段，内置整段录像保持关闭。CI 以 `--headed --video off` 在任务独立 Xvfb 的 1440×1000 显示中运行。Web 在费用报价可见后启动 ffmpeg，确认付款并刷新原意图后用 `q` 结束。Android 通过现有 adb screenrecord 分别录制接受至实际离线、恢复连接至实际保存，只向本次录像的匹配 PID 发 SIGINT。两端都检查录像实际开始、一直存活至片段结束，并用 ffprobe 核对真实 MP4 视频流与时长。提前自行结束，即使退出码为零，也失败。

公开目录只增加 `web-confirm.mp4`、`android-accept-offline.mp4`、`android-resume-save.mp4`，以及各自独立写入的 `web-video.json` 和 `android-video.json`。收据包含原订单与意图、实际退出码和 signal、视频 SHA 与大小。宿主独立复核后合并视频清单。邀请码、钱包密码、恢复文件、完整 attempt 视频和 trace 均不上传。

只检查控制器和阻断规则时运行以下纯自检。它不启动 Node、UI、链或证明，不能替代实际验收。

```sh
python3 scripts/verify-node.py --delivery-ui-selfcheck \
  --binary /usr/bin/true --artifacts .artifacts/paid-ui-selfcheck
```

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
pnpm exec e2e run --config e2e.native.config.ts
```

原生测试通过真实邀请码加入接收网络，检查手机与管理员的安全码相同，再授予管理权限。测试从私有任务文件读取一次性邀请码，像扫码一样以 `vibeface://join?link=` 打开应用。发送方经真实 Node 发出公益订单，原生界面接受并保存文件。测试只读该专用应用的 `Documents/transfers/`，以独立 SHA-256 核对实际保存的字节。没有安装新构建、执行原生界面或读取实际文件时，不能把入口准备完成或 Web 通过作为原生验收成功。

## 让编码代理使用 e2e

项目已配置 Codex、Claude Code 和 Cursor 的 e2e MCP。安装依赖后重新打开可信项目会话。编写或运行测试前，读取 [e2e skill](../.agents/skills/e2e/SKILL.md)。

当前未选择 AI 模型。需要 `agent.act` 时，先选择订阅、API 或本地模型，再按 `pnpm exec e2e guide setup` 配置 provider。ChatGPT 订阅的登录命令为 `pnpm exec e2e login openai`，完成登录后再运行模型测试。不要把凭证写入仓库；CI 保持不调用真实模型。
