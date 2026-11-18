<!--
本页由 Rust 分类表、已提交的 schema 和人工说明生成。
方法说明维护在 crates/bridge/tests/fixtures/codex-method-descriptions.tsv，每行用制表符分隔方法名和说明。
参考页的段落结构维护在 crates/bridge/tests/fixtures/codex-method-reference.md。
从仓库根目录重新生成
VIBEFACE_REGENERATE_CODEX_METHOD_REFERENCE=1 cargo test --locked -p vibeface-bridge --lib methods::reference::regenerate_method_reference -- --ignored --exact
-->

# Codex App Server 方法覆盖表

本表的权限分类、服务端请求兜底策略和通知类别由 `crates/bridge/src/methods.rs` 人工维护。`scripts/codex-schema.py` 从已安装的 Codex CLI 更新 `crates/bridge/schema/app-server-methods.json`。新增方法需要补上 Rust 分类和方法说明，bridge 的测试会核对完整的生成结果。

{{summary}}

协调 Agent 用 `vibeface codex call` 调用 App Server 方法。下表标明该命令拒绝的方法。写方法结果不明时先对账。网页只读 Node 提供的观察状态。

执行的命令、命令输出、diff 和改动文件列表只进观察状态，送往网页前由 `sanitize` 去掉。服务端请求交给协调 Agent 处理，见下文。

## 客户端请求

{{client_requests}}

## 客户端通知

{{client_notifications}}

## 服务端请求

工作线程用 `approvalPolicy=never` 和 `danger-full-access` 启动，正常不会收到审批。Node 订阅的线程上的请求和不属于线程的请求都没有固定策略：Node 挂起请求，记一行日志（方法和线程，不含命令文本），立即唤醒本机协调 Agent。Agent 用 `vibeface requests` 查看、`vibeface respond <request_id> '<result-json>'` 回答；需要本人拍板时用 `vibeface triage raise --request-id` 在负责该线程的 Bot 聊天里提问，提问挂住请求直到有回答。本人答案先持久化，再经私有 `HeldAnswerControl` 回到原执行设备递交，普通 Bot 在提问后不能代答。不属于任何 Bot 的会话先归给主 Bot 再按归属处理；没有 Bot、不属于线程或后台线程（`codex exec`、子代理）上的请求立即兜底。没有协调 Agent、唤醒失败或 120 秒内没有回答也没有提问时，按下表兜底并记录。Node 没有订阅的线程留给原客户端回答。

{{server_requests}}

## 服务端通知

{{server_notifications}}
