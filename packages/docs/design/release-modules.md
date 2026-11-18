# 发布工具模块边界

`scripts/release.py` 只解析并分派现有 13 个 CLI 子命令。库直接 import 定义职责的模块；CLI 不导出库 API。

| 模块 | 职责与依赖 |
| --- | --- |
| `release_format` | 纯格式、版本、精确 pin、资产名、身份校验；contracts 由调用者显式提供 |
| `release_context` | 唯一 ROOT、run/git/blob，以及本地版本、contracts、依赖 pin 读取；依赖 format |
| `release_source` | Hourly 精确生成字节、Git tree、作者与单 parent、main 来源资格；依赖 context/format |
| `release_archive` | 公共确定性归档、解包、seal、平台收据、文件校验；依赖 context/format |
| `release_inventory` | 本地 release.json、完整资产集合与 producer 聚合；依赖 archive/context/format |
| `release_github` | gh/git 传输及响应字节；每次写 primitive 只尝试一次；依赖 archive/context/format |
| `release_publisher` | exact source/CI/tag 门、公开完成证明、immutable 对账与渐进发布；组合上述 owner |
| `release_assembly` | component、native staging、offline compose；先 archive，后 inventory，切断依赖环 |

生产模块不反向 import CLI、hourly 或 tests。跨模块访问 ROOT、run、identity 使用 `release_context.<name>`，不复制全局绑定；fixture patch 定义 owner。库 import 不改变 Python bytecode policy。每个入口在首次本地 import 前关闭 bytecode；shell/workflow 的内联 Python 使用 `-B`。

`hourly.py` 继续拥有输入选择、main CI 调度、单写对账和派发。八个生产 owner 都是六应用的公共构建输入；`tests/release` 不影响应用构建输入。`release_source.expected_tree` 会写 Git objects，验收只在一次性 fixture repository 中运行。

`tests/release/runner.py` 惰性运行原 `release.py self-test` 案例；`test_cli.py` 用真实 CLI 子进程、固定 baseline 和独立 literal/字节 oracle，`test_publisher.py` 用持久 fake gh state 验证未知写的跨 invocation 对账，`test_source.py` 用隔离 Git 对象验证精确生成与来源。fake git/gh/aapt 拒绝未知 argv；环境使用 allowlist，禁用网络边界，不继承凭据。原 train 测试也只修改一次性 fixture Git repository。

最小 CLI 复制包含 `release.py` 和八个 owner；执行 `self-test` 还需 `tests/__init__.py`、`tests/release/__init__.py`、runner 与 test_format_archive。仅校验 BUILD-INFO 的工具只需 format/context 和 `crates/control/src/lib.rs`；format 本身可脱离源码树加载。

归档与 requirements 字节、独立版本和精确 pin、签名后 seal、marker 最后上传、未知写当次不重发，以及 deploy activation transaction 保持原契约。包字段的维护入口见 [CLI 包的构建信息](../releasing.md#cli-包的构建信息)。模块拆分不改变 schema、DATA_EPOCH、协调器 schema，也不授予发布、部署或 tag 操作权限。

Node 发布工具使用方案 A：`release-contract.mjs` 只拥有纯版本/Tag/频道、Gregorian 日历、BigInt 同频道比较、四平台、CLI 包构建信息及 desktop 格式事实；`release-metadata.mjs` 是唯一 Node 源码元数据读取 adapter，严格接受当前声明子集。Rust inner attributes 全部拒绝，独立于相邻 item；注释和 string 内的 attribute 文字不作代码。Rust 常量必须是唯一、顶层、无属性的 u32 十进制 literal；Cargo own version 和 GPUI dependency 必须是显式字符串；Cargo.lock 仅接受当前 package/依赖数组格式，gpui-pre=0.3.12 与 gpui-kit=0.9.1 必须各有唯一记录；JSON manifest 拒绝包括转义键在内的重复 key。Mobile own version 从 expo.extra.releaseVersion 读取，www 独立保持自己的 package version。它不是通用 Rust/TOML parser；不支持的声明需要显式更新 adapter。

build-desktop、verify-desktop 在生产处理内显式读取并传递 contracts/GPUI 源事实，模块初始化不读源码，self-test 使用 literal facts；package-npm 读取实际需要的源事实；publish-npm 只消费纯规则，保留 registry 对账和 publish-once。`node scripts/release-metadata.mjs --app <app>` 只输出只读源码事实 JSON；它不证明 source provenance、编译后的 Rust 常量、制品摘要、签名或跨 job 来源，也不是 producer verified 结果。没有 JSON 缺失 fallback、Rust exporter、跨 workflow metadata artifact 或提交六应用生成版本文件。未来若采用编译常量权威导出，需另行实现并验证来源绑定。

`sealed-runtime.mjs` 的实际文件摘要与 seal 职责不变；verifier 继续读取实际 ZIP、installer、binary/header、签名与 runtime 字节。npm launcher 自含，不依赖仓库模块、Python 或 Rust。`test-release-contract.mjs --cli-fixtures` 在一次性根中用 literal corpus、最小真实 import copy-list、禁网及拒绝未知工具验证真实入口、npm tgz/receipt/wasm 和 Linux desktop ZIP/AppImage；它不证明真实平台签名。九个纯 Node helper/adapter/fixture/四 CLI 路径的单路径变更只选 CI quick-check；windows-signing/private-control 直接变更保留 fallback full gates，Linux import fixture 不代替 Windows ACL/签名检查。完整 AUTO19 16-path 修改仍选 full CI。Hourly release-contract/release-metadata 只影响 CLI/Desktop，windows-signing/private-control 通过 release-core→release-native 的 Windows 签名调用链影响 Core/CLI/Desktop，seal 只影响真实直接消费者 Desktop；tests/corpus 不触发应用发布。

```sh
node scripts/test-release-contract.mjs --cli-fixtures
npm_config_offline=true npm_config_audit=false npm_config_fund=false node scripts/package-npm.mjs --self-test
node scripts/publish-npm.mjs --self-test
node scripts/build-desktop.mjs --self-test
node scripts/verify-desktop.mjs --self-test
```
