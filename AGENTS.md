# 项目约定

- 默认使用中文沟通，不做工作量预估。
- 这是 Rust Cargo workspace。当前目标是统一 Node，按需启用接入、中继、同步、调度及执行。
- 每个人管理本人设备，不设计组织共享设备池或工单调度负责人。公网 Server 是 Node 的角色，也能运行 Agent。
- 调度 SDK 实例与共享业务会话分开。DAG 记录因果历史；同步和恢复不得重放副作用。
- 发布正式版前不保留任何向后兼容，协议、数据格式和 API 直接改成最终形态，不做版本探测、回退、别名或迁移。
- 首个正式版前版本基准保持 `0.0.1`，main 固定携带开发版本 `0.0.1-hourly.2026111300`，各应用按输入变化独立自动发布 `0.0.1-hourly.YYYYMMDDHH`（UTC），使用 `core/app/cli/desktop/mobile/www-v` Tag；CLI/npm 共用 CLI 版本，依赖用精确 manifest pin，不要求组件同版本。`DATA_EPOCH` 保持 `1`，不随功能修复或 hourly 版本递增；确需调整时单独说明，首个正式版前统一为 `1`。
- 账号、设备、事件、附件、路由和命令按个人网络隔离。模型密钥不进入共享记录。
- 网络重试不能重复执行写操作。结果未知时先对账，不当作成功或直接重发。
- 不读取路径含 `trash` 的文件，不打印凭证，Codex 默认 bypass；Codex 发起的请求由协调 Agent 处理，需要本人拍板时才生成拍板卡片或转发 Grok Bot。
- 在独立分支或 worktree 中改代码，只提交本轮修改。本次只创建一个完整新 PR，不推进旧草稿 PR，最终交付测试过的可运行制品。
- 对明确进入交付的改动，要积极主动地 shipping PR（提交、推送并创建 PR）；合并须经用户明确授权。
- 发版、版本递增、创建或推送发布 tag 前，先读 `.agents/skills/release/SKILL.md`；合并或验收通过不构成发布授权。
- 任何新功能在独立分支产生首个提交后，立即 push 并创建 PR。当前开发分支 PR 不自动触发 GitHub Actions，须运行相关本地检查；需要远端验证时手动触发对应 workflow。继续开发，不阻塞等待已启动的远端构建，异步巡查并修复失败。避免频繁本地编译，仅在定位问题或必要验收时执行最小相关验证；最终交付前核对最终 HEAD 的必要检查结果，不将 pending 视为通过。
- 只执行相关验证。安全与同步行为优先在对应 Rust 模块验证，验收须覆盖真实进程及制品。
- 本地 Rust 检查通过 `python3 scripts/local-rust.py run` 执行。每个 checkout 使用自己的 `target`。大型验收制品放在任务 worktree 的 `.artifacts`，planning 保留计划、清单、校验值和报告。任务收口时回收可重建的缓存和已被替代的制品，保留复验所需的输入。
- Web 工作区和 Mobile 的唯一 UI 体系是 `WheatXie/uiarc-pro-reverse` 的 Arc。新增或修改界面先读 `packages/docs/design/uiarc-migration.md`。Web 使用 `packages/uiarc` 中记录来源的组件；Mobile 使用 `apps/mobile/src/components/arc` 的同源原生移植。禁止重新加入 shadcn/ui、React Native Reusables 或另一套组件库、主题和控件样式。
- 颜色、字体、圆角、控件尺寸和动效来自固定版本的 Arc foundation。运行 `pnpm generate:ui` 生成两端 token，不手改生成文件。新增 Arc 项目使用 `scripts/vendor-uiarc.py` 获取来源并记录哈希；原生移植在 `source.json` 标注对应 Arc 项目。UI 改动必须通过 `pnpm check:ui`。
- 主分支 CI 保留核心检查；同步、恢复、调度和工具执行改动须按 `packages/docs/ci.md` 补跑相关本地专项并记录结果。CI 只用受控协议夹具，不调用真实 AI Provider 或读取模型密钥。

- e2e 测试、截图和界面验收只在用户明确要求时编写或运行，不作为默认验收、preflight 或 CI 步骤。被要求时先读 `.agents/skills/e2e/SKILL.md`，启动方式见 `packages/docs/e2e.md`。
