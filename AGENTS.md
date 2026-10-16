# 项目约定

- 默认使用中文沟通，不做工作量预估。
- 这是 Rust Cargo workspace。首版只有 Bridge、Server 和共享协议，不实现 Harness。
- Cursor Agent 运行在本地，由 Bridge 驱动。Server 不运行模型或 Agent。
- 外部控制端与本地 Agent 共用 Bridge 的会话接口。原生 Codex 协议留在 Bridge 内。
- 网络重试不能重复执行写操作。结果未知时先对账，不当作成功或直接重发。
- 不读取路径含 `trash` 的文件，不打印凭证，不自动批准 Codex 审批请求。
- 在独立分支或 worktree 中改代码，通过 PR 交付。只提交本轮修改。
- 只执行与改动相关的验证。新增行为优先在对应 Rust 模块内验证，不创建独立测试文件或 UI。
