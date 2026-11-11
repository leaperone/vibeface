# 移动端聊天首页

## Mode

Operate

## Job

让个人在单手操作下选择 Bot、识别未处理事项，并进入当前对话。

## Reading order

1. 连接与当前网络状态
2. 待处理或恢复提醒
3. 频道和 Bot 列表
4. 最近消息与未读数

## Preserve

- 原生导航、系统 Back、safe area、键盘和动态字体行为
- Bot face 作为身份识别，不让动画成为唯一状态提示
- 现有共享语义主题和 accessibility labels

## Quieting and layout decisions

- 顶部只保留连接、任务、新 Bot、设置这些已存在且可达的动作。
- 待处理事项在列表中优先于普通 Bot，并使用单一强调面。
- 频道标题、Bot 标题和元信息保持明确的大小/重量阶梯；减少多余容器。
- 列表行保持 44px 以上触控高度，分隔线只用于连续列表。
- 聊天 composer 使用现有 material fallback；不增加装饰性玻璃层。

## Required states

loading, empty, offline, waiting, recovery, unread, reduced motion, reduced transparency, keyboard open, long text.

## Acceptance evidence

需要在 iOS 与 Android 的实际模拟器/设备上检查窄屏、宽窗口、深色模式、较大字体、系统 Back、键盘和 reduced-motion/transparency。
