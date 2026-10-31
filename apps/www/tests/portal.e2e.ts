import { test } from '@e2e-dev/web'
import { expect } from 'e2e'

test('portal serves docs and searches Chinese content', async ({ app, screen, browser }) => {
  await browser.setViewport({ width: 1280, height: 900 })
  await app.open('/')
  await expect(screen.getByRole('heading', { name: /用聊天，指挥\s*自己的电脑。/ })).toBeVisible()

  await screen.getByRole('link', { name: '开始安装' }).click()
  await expect(screen.getByRole('heading', '安装与管理节点')).toBeVisible()
  await screen.getByRole('button', { name: '打开文档搜索' }).first().click()
  await screen.getByRole('combobox').fill('恢复包')
  await expect(screen.getByRole('option').filter({ hasText: '恢复包' }).first()).toBeVisible()

  await app.open('/')
  await screen.getByRole('link', { name: '文档' }).click()
  await expect(screen.getByRole('heading', 'Vibeface 能帮你做什么')).toBeVisible()
  await screen.getByRole('link', { name: '与原生工具及 OpenClaw 的比较' }).click()
  await expect(screen.getByRole('heading', '已有 Agent，为什么还用 Vibeface')).toBeVisible()

  await screen.getByRole('button', { name: '打开文档搜索' }).first().click()
  await screen.getByRole('combobox').fill('事件的发生顺序')
  await screen.getByRole('option').filter({ hasText: '事件的发生顺序' }).first().click()
  await expect(browser).toHaveURL(/\/docs\/history-sync\/?(?:#.*)?$/)
  await expect(screen.getByRole('heading', '换设备时，哪些历史会同步')).toBeVisible()

  await screen.getByRole('button', { name: '打开文档搜索' }).first().click()
  await screen.getByRole('combobox').fill('被盗管理设备')
  await screen.getByRole('option').filter({ hasText: '被盗管理设备' }).first().click()
  await expect(browser).toHaveURL(/\/docs\/privacy-security\/?(?:#.*)?$/)
  await expect(screen.getByRole('heading', '谁能读到内容，权限在哪里生效')).toBeVisible()
  await screen.getByRole('link', { name: 'Bot 与任务执行' }).click()
  await expect(screen.getByRole('heading', 'Bot 如何协调，Agent 在哪里执行')).toBeVisible()

  await app.open('/security')
  await screen.getByRole('button', { name: '切换到深色主题' }).click()
  await expect(screen.getByRole('button', { name: '切换到浅色主题' })).toBeVisible()
  await app.open('/')
  await browser.setViewport({ width: 390, height: 844 })
  await expect(screen.getByRole('link', { name: '文档' })).toBeVisible()
  await expect(screen.getByRole('link', { name: '下载' })).toBeVisible()

  await app.open('/404.html')
  await expect(screen.getByRole('heading', '这条路径没有内容。')).toBeVisible()
})
