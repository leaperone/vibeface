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

每次运行创建独立 Node 数据目录，使用随机本机端口，关闭模型执行和 Codex 会话扫描。测试结束后停止自己的进程，不连接已有 Node。数据、日志、报告和下载文件位于 `.artifacts/e2e/`，不会提交。恢复密码每次随机生成，通过 e2e secrets 传入。

示例检查建网页面。核心测试依次建网、下载恢复文件、跳过 AI 设置、创建 Bot，再刷新确认它仍存在。这两项测试不需要模型或登录。CI 使用本次提交打包出的 Linux 程序运行。

Node 按整次命令隔离。重复验收请重新运行命令，不使用 `--repeat-each`，也不要开启重试；新增会修改后端状态的流程时，应先改为按测试隔离 Node。

只运行核心测试或显示浏览器：

```sh
pnpm exec e2e run tests/onboarding.e2e.ts
pnpm test:e2e --headed
```

排查失败时，读取终端指向的 `trace.md`。不要上传 Node 数据目录或恢复文件。

## 让编码代理使用 e2e

项目已配置 Codex、Claude Code 和 Cursor 的 e2e MCP。安装依赖后重新打开可信项目会话。编写或运行测试前，读取 [e2e skill](../.agents/skills/e2e/SKILL.md)。

当前未选择 AI 模型。需要 `agent.act` 时，先选择订阅、API 或本地模型，再按 `pnpm exec e2e guide setup` 配置 provider。ChatGPT 订阅的登录命令为 `pnpm exec e2e login openai`，完成登录后再运行模型测试。不要把凭证写入仓库；CI 保持不调用真实模型。
