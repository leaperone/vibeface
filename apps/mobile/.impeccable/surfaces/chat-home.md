# 移动端聊天首页

## Mode

Operate

## Job

让个人单手选择 Bot、识别待处理事项，并进入真实对话。

## Reading order

1. 当前网络与连接状态
2. 待处理或恢复提醒
3. 频道和 Bot 列表
4. 最近消息、时间与未读数

## Arc decisions

- 统一使用固定提交 `859606819c68787b2dccb54adcb9b08bd3272d5d` 的 Arc 原生移植组件和生成主题。
- 窄屏保留消息、任务、设置三个根标签。待处理留在任务页顶部。宽屏使用 Arc sidebar 的中性侧栏和真实路由。
- 使用 Inter 16/14、400/500 字重与 Arc 间距、圆角和动效；原生大标题使用 Geist。
- Bot face 保留身份与状态，去掉背景光晕和渐变。未读数和活动控件使用 Arc 中性强调色。
- 设置行使用普通图标和分隔线；卡片只表达真实内容边界。
- Chat composer 在两个平台使用一个 Arc ai-chat 的实色边框输入面板，保留附件选择、停止、发送与待确认状态。
- 共享写入结果未知时保留查询、对账与安全重试流程。

## Preserve

原生路由、Back 和边缘返回手势、安全区域、键盘位置、动态字体、辅助功能标签、原生扫码与文件/照片/分享/粘贴动作。所有产品文案、数据和操作条件保留。

## Required states

loading, empty, offline, waiting, recovery, unread, unknown delivery, reduced motion, keyboard open, long text, light and dark.

## Evidence scope

本轮使用 TypeScript、现有行为测试、来源映射、主题检查和轻量生产导出验证。没有运行模拟器、截图或界面验收。编译和源码审计不代表设备上的视觉及辅助功能验收已完成。
