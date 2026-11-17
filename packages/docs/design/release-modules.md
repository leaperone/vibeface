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

归档与 requirements 字节、九字段 BUILD-INFO、独立版本和精确 pin、签名后 seal、marker 最后上传、未知写当次不重发，以及 deploy activation transaction 保持原契约。模块拆分不改变 schema、DATA_EPOCH、协调器 schema，也不授予发布、部署或 tag 操作权限。
