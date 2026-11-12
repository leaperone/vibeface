# Web 工作区

## Mode

Operate

## Job

让个人快速看懂当前 Bot/任务状态，并完成下一步操作。

## Reading order

1. 当前上下文（Bot、任务或频道）
2. 需要注意的等待、错误或恢复状态
3. 对话/任务内容
4. 一个明确的下一步动作

## Preserve

- 侧栏导航、键盘顺序、消息与任务的独立上下文
- 未知写入结果先对账的文案和状态
- 现有主题、Bot face、焦点环和响应式断点

## Quieting and layout decisions

- 让标题、上下文和动作共享同一中列对齐线。
- 使用邻近关系和分隔线表达列表分组，避免增加嵌套卡片。
- 保留等待、恢复、离线的颜色，但始终配合文字或图标。
- 页面只保留一个主任务路径；辅助动作进入现有 header action slot。
- 宽屏使用侧栏，窄屏使用返回和抽屉；不引入新的导航模式。

## Required states

loading, empty, offline, waiting, recovery, failed, keyboard focus, narrow viewport, long text.

## Acceptance evidence

浏览器渲染检查需覆盖宽屏、窄屏、深色模式、键盘焦点、长文本和待处理状态。
