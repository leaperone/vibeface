---
title: 在终端使用 Bot
description: 用 CLI 完成加入、聊天、附件、任务控制及客户端业务操作。
---

# 在终端使用 Bot

CLI 连接与 Web 相同的 Core。你可以在终端持续聊天，也可以让脚本或另一个 Agent 向同一个 Bot 发送消息、等待结果和回答提问。

## 准备客户端身份

启动本机 Node：

```sh
vibeface
```

Node 尚未加入网络时，创建个人网络并保存恢复文件：

```sh
vibeface client create-network --name '我的终端' --node-name '我的电脑' --recovery-file /absolute/path/recovery.json
```

命令分别询问客户端密钥口令与恢复密码。已有网络使用管理客户端生成的加入链接：

```sh
vibeface client enroll '<加入链接>' --name '我的终端'
```

在管理设备核对安全码后批准。CLI 也能完成这一步：

```sh
vibeface client requests
vibeface client approve <join-id> --code <六位安全码>
```

`--client-dir` 选择客户端身份目录，`--profile home` 在该目录下使用独立的命名身份。自动化调用通过 `--passphrase-fd` 传入口令，命令参数不接收明文口令。

`--url` 选择 Gateway，`--node` 钉住目标 Node 的设备编号。已加入的客户端默认使用记住的入口；也可以经同一个 Gateway 访问另一台本人的 Node。`client remember-entry --url URL --node DEVICE_ID` 在验证目标成员身份后保存选择。恢复到已初始化的远端 Node 时，也可以用这两个参数选择目标，无需本地运行 Node。

不需要打开浏览器。当前 `--no-web` 会同时关闭客户端 API，因此使用 CLI 客户端时保留 API 监听。网页是否打开不影响 CLI 的身份和会话。

## 与同一个 Bot 聊天

列出或创建 Bot：

```sh
vibeface bots list
vibeface bots create --name Assistant --instructions '帮助我处理自己的项目和设备任务'
vibeface chat --bot Assistant
```

`--bot` 接受准确名称或 Bot ID。名称重名时，命令列出候选 ID。`--conversation` 可以直接选择已有会话；同时给出两个参数时，它们必须指向同一个 Bot。

输入消息后，CLI 显示该输入对应的进度、回复和待决定问题。交互命令包括：

```text
/attach /absolute/path/report.txt
/questions
/answer <question-id> <回答>
/stop-work <work-id>
/stop-reply message:<message-id>
/history
/quit
```

`/quit`、EOF 和 Ctrl-C 断开客户端。停止回复与停止工作分别使用对应命令，退出终端不表示远端工作已停止。

## 从脚本发送消息

单次发送并等待对应回复：

```sh
vibeface chat --bot Assistant --message '在我的 Mac 上检查项目测试' --message-id check-mac-tests-1 --wait --timeout 600 --json
```

从文件或标准输入读取消息：

```sh
vibeface chat --bot Assistant --text-file /absolute/path/request.txt --wait --json
vibeface chat --bot Assistant --stdin --wait --json
```

机器模式按行输出 JSON，包括持久操作与最终等待状态。提示与连接诊断写入标准错误。`accepted` 表示输入已接收，不能据此认为工作完成。遇到需要决定的问题、结果未知或等待超时时，命令保留原始消息 ID，并以非零状态结束。超时不重发输入。

重试同一输入时保留原 `--message-id`、内容、目标和附件。同 ID 改内容会被拒绝。未知结果先查询原操作：

```sh
vibeface client pending --json
vibeface client reconcile <operation-id> --json
```

## 发送和保存文件

将文件附在消息中：

```sh
vibeface chat --bot Assistant --message '检查这个文件' --attach /absolute/path/report.txt --json
```

从该会话的历史取得附件哈希，再保存文件：

```sh
vibeface client files get --conversation <conversation-id> --hash <sha256> --output /absolute/path/report.txt
```

CLI 在校验完整大小与 SHA-256 后发布输出文件。已有文件需要显式 `--force`。实验室文件交付使用 `client files prepare-delivery` 与 `client files save-delivery`，其开关、条款与身份要求沿用 [文件交付](paid-delivery.md)。

工作区或队列编辑需要附件引用时，用 `client files stage --file PATH --owner DRAFT` 取得 `hash`、`size` 和 `name`，再放进有类型的业务输入。客户端在提交操作时上传已暂存的内容。`client files release --owner DRAFT` 释放本地草稿引用，尚未结束的持久操作仍保留它们需要的文件。

## 使用其他业务功能

查询完整命令与参数示例：

```sh
vibeface client --help
vibeface client read --help
vibeface client write --help
vibeface client capabilities --json
```

读取操作和持久写入各有具名命令：

```sh
vibeface client read nodes --json
vibeface client read questions --json
vibeface client read conversation --input '{"conversation_id":"<conversation-id>"}' --json
vibeface client write answer --input '{"conversation_id":"<conversation-id>","question_id":"<question-id>","text":"继续"}' --json
```

复杂业务参数接受 JSON、`@FILE` 或 `-` 标准输入。具名命令决定操作类型，输入不填写外层 `type`。命令在连接前检查参数结构并拒绝未知字段。工作区、频道、记忆、定时工作、设置和实验室操作的完整示例由 `client capabilities` 输出。

持续观察状态使用只读连接，不占用同一身份的持久写入队列：

```sh
vibeface client watch --conversation <conversation-id>
```

事件流在首次连接及重连后输出状态快照。`resync_required` 表示需要刷新持久状态，事件流不承诺补放所有临时进度。

## 管理加入与恢复

查询或取消由终端发起的本机 Node 加入：

```sh
vibeface client node-join-status
vibeface client node-join-cancel
```

检查加入链接使用 `client node-join-check -` 从标准输入读取。撤回当前客户端尚未结束的申请使用 `client withdraw-enrollment`。这些命令操作原请求，不创建新的加入申请。已先完成批准时，状态仍是已加入。

`client rotate-recovery` 保存替换恢复文件并轮换恢复密钥，保留现有成员。未知设置操作使用 `client settings-review` 查看脱敏意图；核对实际 Node 设置后，将输出的 `review_hash` 和说明交给 `client settings-close`，不会再次发送设置。

`client recover --members @members.json` 选择恢复后保留的设备。文件是成员数组，每项包含 `device_id`、`kind`、`name` 和 `manage`，使用恢复审核中已核验的设备身份与名称。保留当前连接的 Node；当前 CLI 的新管理身份会自动加入。新建恢复请求未指定 `--members` 时保留全部现有成员。恢复计划已经保存后，命令继续原计划；显式传入不同成员、名称、目标或替换文件会被拒绝。

先用 `client recover --review --recovery-file PATH` 读取已核验的候选成员。预览不创建替换备份或恢复计划，也不提交恢复。保存的身份操作可通过 `client identity-plan status` 查看、`resume` 续办原请求、`discard` 显式移除尚未发送或已经结清的日志。未知或等待中的请求不能丢弃。身份操作确认成功后自动清理已应用的日志，保留恢复备份。

创建一次性决定令牌时，`client write create-decision-token` 必须指定 `--secret-output FILE`。普通 JSON 结果只包含操作状态与文件路径，令牌保存到私密文件。
