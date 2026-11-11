---
title: 状态与恢复约束
description: 定位执行、输入、连接和恢复的状态模型与安全边界。
---

# 状态与恢复约束

业务状态可能跨越持久记录、运行中的进程和客户端视图。枚举中的终态不代表这些维度同时完成。
修改状态转换时，先核对下表中的事实来源与不变量，再检查调用方和恢复路径。

| 领域 | 事实来源 | 必须保留的约束 |
| --- | --- | --- |
| 执行 | `apps/core/src/work.rs`、`store/work.rs` | 终态不因迟到事件倒退。重启对账，未知结果不重发。 |
| 调度 | `apps/core/src/scheduling.rs`、`store/scheduler.rs` | Job、原生 turn、执行权限与工具记录共同决定能否继续。 |
| 活跃输入 | `apps/core/src/runtime/executor.rs`、`runtime/input_receipts.rs` | 接收、关联原生 turn 与完成是不同事实。安全重试须有原输入的精确证据。 |
| 跨设备命令 | `apps/core/src/store/admission.rs`、`store/mailbox.rs` | 同业务 ID 不得改变意图。重复投递不得重复执行。 |
| 同步 | `apps/core/src/runtime/peers.rs`、`store/sync.rs` | 导入历史不触发副作用。分叉与未知结果保留证据。 |
| 加入 | `apps/core/src/client.rs`、`identity/local.rs` | 重复查询返回已有任务。只有相同发起者可以替换其未完成加入。 |
| 身份恢复 | `crates/secure/src/membership.rs` | 当前授权由签名历史和时间投影。迟到但有效的 veto 仍须参与投影。 |
| Provider | `apps/core/src/providers.rs`、`crates/bridge/src` | 可启动、已认证、目录可用、已开始执行分别判断。 |
| 资金与文件交付 | `apps/core/src/delivery` | 订单、链上支付、证明和逐窗状态须一致。链状态未知或重组后撤回旧完成声明。 |
| 客户端连接 | `packages/client/src/lib/link.ts`、`identity/context.tsx` | 保留旧画面不等于新连接已授权。旧响应不能修改新会话。 |
| 客户端发件箱 | `packages/client/src/lib/outbox.ts`、`workspace/context.tsx` | 保存原始请求。提交结果未知时先对账。传输接收不等于 Agent 执行成功。 |
| 草稿与附件 | Web 和 Mobile 的 chat composer | 旧保存或选择结果不得清除用户的新草稿，或进入另一个目标。 |
| 桌面进程 | `apps/desktop/src/main/lifecycle.ts`、`session.ts` | 已连接的外部服务不归当前窗口所有。退出未确认时保留所有权屏障。 |
| 本机更新 | `apps/cli/src/update`、`apps/core/src/maintenance.rs` | 未确认更新保留日志和执行屏障。恢复运行程序不回滚业务数据。 |

## 加入任务的所有权

加入任务同时包含邀请、发起者、进度和取消句柄。替换任务会取消旧请求并清理其本地承诺文件。
判断能否替换与安装新任务必须在同一临界区内完成。
如果权限或成员检查需要等待，等待之后必须重新核对当前任务。
同发起者、同邀请的请求只读取原任务进度。不同发起者不得取消仍在进行的任务。

## 选择改造方式

优先复用现有领域状态和转换函数。只有存在可证明的非法组合或重复规则时才收敛数据结构。
独立事实保留独立维度，尤其是保存、接收、执行、完成与清理。
类型校验、恢复测试、真实进程及界面验收分别提供不同证据，不能互相替代。
