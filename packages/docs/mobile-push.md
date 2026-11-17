---
title: 移动端后台推送
description: Expo 设备推送的构建配置、认证注册契约与交付边界。
---

# 移动端后台推送

移动 App 使用 Expo Notifications 获取设备推送令牌，向当前入口 Node 注册。
Node 在 App 挂起或结束后仍可向 Expo 提交 Bot 回复和问题提醒；实际展示依赖
APNs、FCM、系统通知权限和系统通知设置。前台连接仍通过原有通知列表获取未读内容。

## 构建配置

`apps/mobile/app.config.ts` 读取 `EXPO_PUBLIC_EAS_PROJECT_ID` 作为 EAS 项目 ID，
读取 `GOOGLE_SERVICES_JSON` 作为 Android Firebase 配置文件路径，并启用
`expo-notifications` 插件。构建前需要在所属 Expo/EAS 项目配置有效的 APNs 与
FCM 推送凭据，Android 构建需要对应应用的 Firebase 配置。项目 ID 和配置文件
不能替代推送凭据。iOS 分发签名见 [iOS 构建与分发](ios.md)。

当前代码检查使用受控夹具，没有向真实 Expo 设备发送通知。尚未提供的
APNs、FCM 或 EAS 凭据需要由应用所有者配置；代码支持不代表实体设备通达已验收。

## 注册契约

认证设备通过 RPC 调用以下方法；`client_id` 由 Noise 会话注入，客户端不能
为其他设备注册。只有网络级用户可以调用，Agent 和会话级访问均被拒绝。

```json
{
  "method": "register_push",
  "token": "ExpoPushToken[device-token]",
  "sound": true,
  "badge": true,
  "muted_conversations": []
}
```

成功返回 `registered: true` 和 `lease_expires_at_ms`。租约为 30 天，App 需在
到期前重新注册；换入口后需向新入口注册。令牌接受 `ExpoPushToken[...]` 或
`ExponentPushToken[...]`，总长最多 256 字节；静音列表最多 256 个有效会话 ID。
`unregister_push` 无需其他参数，返回 `registered: false`。

首次注册及停用后重新注册时，Node 将当前未读设为基线，避免补发历史提醒。
更新令牌和偏好保留已处理记录。静音期间的提醒不会在取消静音后补发。
声音开启时使用 Android `vibeface` channel，关闭时使用 `vibeface-silent`；
系统 channel 设置仍由用户控制。启用角标时，数值为该设备经过静音及会话过滤后
的可见未读数；提交推送不会确认已读，也不会执行问题中的操作。

## 投递边界

订阅、去重和重试状态只保存在当前 Node 的私有数据库 `identity_meta`，
绑定个人网络与认证设备，不进入 DAG、共享历史或日志。Node 重启恢复状态，
投影重建不会清空已处理记录。每次发送前重查成员权限、注册状态、租约和未读；
设备或入口 Node 被撤销、换网、注销或租约过期后不再提交新推送。
已经被 Expo 接收的通知不能撤回。

Node 固定向 `https://exp.host/--/api/v2/push/send` 提交最小提示，不发送回复或
问题正文。`data.destination` 包含 `network_id`、`device_id`、`notification_id`、
`conversation_id`，供点击后回到对应会话。通知有效期为 5 分钟，Android tag
与 collapse ID 使用稳定的通知 ID。

成功 ticket 只证明 Expo 已接收，不能证明 APNs/FCM 接收或设备已展示。
本实现不查询 delivery receipts；ticket 返回 `DeviceNotRegistered` 时注销令牌。
网络错误、拒绝、无效响应和其他 ticket 错误保留待重试状态，按指数退避重试，
最长间隔一小时，不将暂时失败标记成功。

注册状态只在当前入口保存。多个 Node 同时持有仍有效的注册、HTTP 结果未知
或进程在 Expo 接收后落盘前退出时，可能重复提醒；稳定 tag 和 collapse ID
用于限制重复展示，不提供 exactly-once 保证。切换入口时应注销旧入口。
