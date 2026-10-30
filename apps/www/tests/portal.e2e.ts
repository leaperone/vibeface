import { test } from '@e2e-dev/web'
import { expect } from 'e2e'

test('portal serves docs and searches Chinese content', async ({ app, screen, browser }) => {
  await browser.setViewport({ width: 1280, height: 900 })
  await app.open('/')
  await expect(screen.getByRole('heading', '把 Agent 带回你自己的设备。')).toBeVisible()

  await app.open('/docs/install/')
  await expect(screen.getByRole('heading', '安装与管理节点')).toBeVisible()
  await screen.getByRole('button', { name: '打开文档搜索' }).first().click()
  await screen.getByRole('combobox').fill('恢复包')
  await expect(screen.getByRole('option').filter({ hasText: '恢复包' }).first()).toBeVisible()

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
