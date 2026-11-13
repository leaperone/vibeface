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
| 草稿与附件 | Web 和 Mobile 的 chat composer、外部聊天设置 | 失败不清空未保存草稿。成功只清除对应的已提交内容。旧选择结果不得进入另一个目标。 |
| LAN 与 mesh 发现 | `apps/core/src/net/lan.rs`、`net/mesh.rs` | 广播只是地址提示，连接仍需认证。停止后不因 refresh 复活。重连退避随邻居状态更新。 |
| 原生密码学适配 | `packages/secure-native`、`crates/secure-ffi` | 握手失败关闭，完成时消费原握手。接收、关闭与运行时重建需核对句柄归属。 |
| 系统服务 | `scripts/install-node-service.sh`、`scripts/install-node-service.ps1` | 服务管理器已加载或受理启动不等于 Core 健康。停止须确认进程与数据锁均已释放。 |
| 官网主题 | `apps/www/src/components/theme-toggle.tsx` | hydration、偏好与解析后的颜色分别处理。持久化由主题 provider 管理。 |
| 桌面进程 | `apps/desktop/src/host/lifecycle.rs`、`apps/desktop/src/host/mod.rs` | 已连接的外部服务不归当前窗口所有。退出未确认时保留所有权屏障。 |
| 本机更新 | `apps/cli/src/update`、`apps/core/src/maintenance.rs` | 未确认更新保留日志和执行屏障。恢复运行程序不回滚业务数据。 |

## 加入任务的所有权

加入任务同时包含邀请、发起者、进度和取消句柄。替换任务会取消旧请求并清理其本地承诺文件。
判断能否替换与安装新任务必须在同一临界区内完成。
如果权限或成员检查需要等待，等待之后必须重新核对当前任务。
同发起者、同邀请的请求只读取原任务进度。不同发起者不得取消仍在进行的任务。

## 同设备输入保存

分配序号、签名完成和进入耐久发件箱是不同状态。序号已分配但尚未保存的输入，对发件箱发送者还不可见。
多个浏览器页面共享设备身份和存储时，同目标的保存通过 Web Locks 排队，覆盖附件准备、签名和发件箱写入。
所有目标共享的序号表另用短锁读改写，不把某个目标的慢签名带入其他目标。
锁不等待远端 RPC 完成；保存后的发送、未知结果对账仍沿用原发件箱状态。
原生单运行时保留现有页面内的目标队列，平台能力通过 `Platform.inputLock` 提供。

## 选择改造方式

优先复用现有领域状态和转换函数。只有存在可证明的非法组合或重复规则时才收敛数据结构。
独立事实保留独立维度，尤其是保存、接收、执行、完成与清理。
类型校验、恢复测试、真实进程及界面验收分别提供不同证据，不能互相替代。

## 验证具体转换

加入任务的并发归属、同邀请重试和取消有对应的 Rust 回归入口：

```sh
cargo test -p vibeface-core --lib concurrent_node_joins_preserve_the_first_owner_and_request
cargo test -p vibeface-core --lib a_terminal_join_runs_in_the_running_node_and_asking_again_only_reads_it
cargo test -p vibeface-core --lib the_local_page_cancels_a_waiting_join_but_not_an_approved_one
```

外部聊天密钥的失败、成功与保存期间再次编辑，使用实际 Web 和 Mobile 组件的回调验证：

```sh
node --experimental-strip-types --test --test-name-pattern='webhook secret saves' apps/app/src/features/workspace/entry-url.test.ts
```

组件检查不验证浏览器或原生设备渲染。修改连接或执行路径时，还须选择对应的真实进程验收；界面验收只在明确要求时执行。
