import assert from 'node:assert/strict'
import { expect, secrets } from 'e2e'
import { test } from './fixtures'

test('keeps workspace navigation and device actions usable on narrow screens', { timeout: 180000 }, async ({ app, node, screen, browser }) => {
  await browser.setViewport({ width: 390, height: 844 })
  await app.open(node.entry)
  await screen.getByRole('button', '新建我的网络').click()
  await screen.getByLabel('这个节点叫什么？').fill('界面验收工作站')
  await screen.getByRole('button', '下一步').click()
  await screen.getByLabel(/^恢复密码/).fill(secrets.get('recoveryPassword'))
  await screen.getByRole('button', '下一步').click()
  await browser.waitForDownload(() => screen.getByRole('button', '下载恢复文件').click())
  await screen.getByRole('checkbox', '已把文件和密码分开保存').check()
  await screen.getByRole('button', '完成创建').click()
  await screen.getByRole('button', '稍后设置').click()
  await screen.getByLabel('名称', { exact: true }).fill('界面验收助手')
  await screen.getByRole('button', '完成', { exact: true }).click()
  await expect(browser).toHaveURL(/\/b\/[^/?#]+$/)
  await expect(screen.getByText('Bot 还不能回复', { exact: true })).toBeVisible()
  await app.screenshot('quality-chat-narrow')

  await browser.goto('/devices')
  await expect(screen.getByRole('heading', '我的设备', { level: 1 })).toBeVisible()
  await expect(screen.getByRole('button', '添加设备', { exact: true })).toHaveCount(1)
  for (const width of [320, 390, 768, 1440]) {
    await browser.setViewport({ width, height: 900 })
    const geometry = await browser.evaluate(() => {
      const heading = document.querySelector('main > header h1')
      const header = heading?.parentElement
      if (!(heading instanceof HTMLElement) || !header) throw new Error('Device page heading is missing')
      const title = heading.getBoundingClientRect()
      return {
        titleHeight: title.height,
        titleWidth: title.width,
        viewport: innerWidth,
        controls: [...header.querySelectorAll('button')].filter(button => button.getBoundingClientRect().width > 0).map(button => {
          const box = button.getBoundingClientRect()
          return { left: box.left, right: box.right, width: box.width, height: box.height }
        }),
      }
    })
    assert(geometry.titleHeight <= 40, 'The device title stays on one readable line')
    assert(geometry.titleWidth >= 96, 'The title retains enough width for its label')
    for (const box of geometry.controls) {
      assert(box.left >= 0 && box.right <= geometry.viewport, 'Header controls remain inside the viewport')
      assert(box.height >= 44, 'Header controls retain touch-sized hit areas')
    }
    await app.screenshot(`quality-devices-${width}`)
  }
  await browser.setViewport({ width: 390, height: 844 })
  await browser.goto('/tasks')
  await expect(screen.getByText('还没有任务对话', { exact: true })).toBeVisible()
  await expect(screen.getByRole('group', '任务状态')).toHaveCount(0)
  await expect(screen.getByRole('heading', '当前记录没有未完成的任务', { exact: true })).toHaveCount(0)
  const newTask = screen.getByRole('button', '新任务', { exact: true })
  await expect(newTask).toBeEnabled()
  const newTaskBox = await newTask.boundingBox()
  assert(newTaskBox && newTaskBox.y + newTaskBox.height <= 844, 'The first task action is visible without scrolling')
  await app.screenshot('quality-empty-tasks-narrow')
  await newTask.click()
  await screen.getByLabel('任务名称', { exact: true }).fill('保存任务并检查筛选')
  await screen.getByLabel('交代内容', { exact: true }).fill('这台受控设备未开启 AI，仅验收任务保存与列表。')
  await screen.getByRole('button', '创建并发送', { exact: true }).click()
  await expect(browser).toHaveURL(/\/tasks\/[^/?#]+$/)
  const contextOpener = screen.getByRole('button', '上下文和历史', { exact: true })
  await contextOpener.click()
  const contextDialog = screen.getByRole('dialog', '任务上下文和历史', { exact: true })
  await expect(contextDialog).toBeVisible()
  const contextBox = await contextDialog.boundingBox()
  assert(contextBox && contextBox.height >= 700, 'Mobile context has a dedicated reading surface')
  await expect(screen.getByText('这个任务尚未运行；实际执行后才会提供本轮上下文。', { exact: true })).toBeVisible()
  await browser.keyboard.press('Escape')
  await expect(screen.getByRole('dialog')).toHaveCount(0)
  await expect(contextOpener).toBeFocused()
  const taskHistoryBox = await browser.locator('[data-conversation-history]').boundingBox()
  assert(taskHistoryBox && taskHistoryBox.height >= 160, 'Closing context restores the task reading area')
  const taskSearch = screen.getByLabel('搜索当前任务已加载的记录', { exact: true })
  await expect(screen.getByText(/^已加载最近 \d+ 条/)).toBeVisible()
  await taskSearch.fill('quality-no-match-task-8c02')
  await expect(screen.getByRole('heading', '没有符合关键词的记录', { exact: true })).toBeVisible()
  await expect(screen.getByRole('heading', '发送一条交代开始对话', { exact: true })).toHaveCount(0)
  await screen.getByRole('button', '清除搜索', { exact: true }).click()
  await expect(taskSearch).toHaveValue('')
  await expect(screen.getByRole('heading', '没有符合关键词的记录', { exact: true })).toHaveCount(0)
  await browser.setViewport({ width: 320, height: 740 })
  await browser.addInitScript(() => {
    ;(window as unknown as { qualityLayoutShifts: number[] }).qualityLayoutShifts = []
    new PerformanceObserver(list => { for (const entry of list.getEntries()) {
      const shift = entry as PerformanceEntry & { hadRecentInput: boolean; value: number }
      if (!shift.hadRecentInput) (window as unknown as { qualityLayoutShifts: number[] }).qualityLayoutShifts.push(shift.value)
    } }).observe({ type: 'layout-shift', buffered: true })
  })
  await browser.goto('/tasks')
  await expect(screen.getByRole('group', '任务状态')).toBeVisible()
  await browser.evaluate(async () => { await document.fonts.ready; await new Promise(resolve => requestAnimationFrame(() => requestAnimationFrame(resolve))); return true })
  const filledTasks = await browser.evaluate(() => ({
    cls: (window as unknown as { qualityLayoutShifts: number[] }).qualityLayoutShifts.reduce((sum, value) => sum + value, 0),
    overflow: [...document.querySelectorAll<HTMLElement>('main p')].filter(element => element.getBoundingClientRect().height > 2 && element.getBoundingClientRect().width > 2 && element.scrollWidth > element.clientWidth + 1).map(element => element.innerText),
  }))
  assert(filledTasks.cls <= .1, `The filled task page remains stable on first read (${filledTasks.cls})`)
  assert.deepEqual(filledTasks.overflow, [], 'Task text fits its own reading area on a narrow screen')
  await app.screenshot('quality-filled-tasks-320')
  await screen.getByRole('button', '进行中', { exact: true }).click()
  await expect(screen.getByRole('heading', '没有符合筛选的任务', { exact: true })).toBeVisible()
  await expect(screen.getByText('还没有任务对话', { exact: true })).toHaveCount(0)
  await screen.getByRole('button', '清除筛选', { exact: true }).click()
  await expect(screen.getByRole('heading', '没有符合筛选的任务', { exact: true })).toHaveCount(0)
  await browser.setViewport({ width: 390, height: 844 })
  await expect(screen.getByText('读取最新内容没有成功。再试一次。', { exact: true })).toHaveCount(0)
  const filterControls = await browser.evaluate(() => {
    const canvas = document.createElement('canvas')
    canvas.width = canvas.height = 1
    const paint = canvas.getContext('2d', { willReadFrequently: true })
    if (!paint) throw new Error('Canvas color conversion is unavailable')
    const color = (value: string) => {
      paint.clearRect(0, 0, 1, 1)
      paint.fillStyle = value
      paint.fillRect(0, 0, 1, 1)
      return [...paint.getImageData(0, 0, 1, 1).data]
    }
    const luminance = (rgb: number[]) => rgb.slice(0, 3).map(value => {
      const channel = value / 255
      return channel <= 0.04045 ? channel / 12.92 : ((channel + 0.055) / 1.055) ** 2.4
    }).reduce((sum, channel, index) => sum + channel * [0.2126, 0.7152, 0.0722][index], 0)
    return [...document.querySelectorAll<HTMLElement>('[role="group"][aria-label="任务状态"] button')].map(button => {
      let parent: HTMLElement | null = button
      let background = [0, 0, 0, 0]
      while (parent && background[3] !== 255) {
        background = color(getComputedStyle(parent).backgroundColor)
        parent = parent.parentElement
      }
      if (background[3] !== 255) throw new Error('Filter background is not observable')
      const foreground = luminance(color(getComputedStyle(button).color))
      const backdrop = luminance(background)
      return { label: button.innerText, height: button.getBoundingClientRect().height, contrast: (Math.max(foreground, backdrop) + 0.05) / (Math.min(foreground, backdrop) + 0.05) }
    })
  })
  assert.equal(filterControls.length, 4)
  for (const control of filterControls) {
    assert(control.height >= 44, `${control.label} has a touch-sized target`)
    assert(control.contrast >= 4.5, `${control.label} meets normal-text contrast`)
  }
  await app.screenshot('quality-tasks-narrow')
  await browser.goto('/channels')
  await screen.getByRole('button', '新频道', { exact: true }).first().click()
  await screen.getByLabel('频道名称', { exact: true }).fill('手机历史验收')
  await screen.getByLabel('频道目标', { exact: true }).fill('检查频道说明及历史在最窄屏下仍可阅读。')
  await screen.getByRole('button', '创建频道', { exact: true }).click()
  await expect(browser).toHaveURL(/\/channels\/[^/?#]+$/)
  await browser.setViewport({ width: 320, height: 740 })
  await expect(screen.getByText(/^已加载最近 \d+ 条/)).toBeVisible()
  const historyBox = await browser.locator('[data-conversation-history]').boundingBox()
  assert(historyBox && historyBox.height >= 160, 'Channel history retains a useful reading area at 320px')
  await app.screenshot('quality-channel-history-320')
  const channelSearch = screen.getByLabel('搜索当前频道已加载的记录', { exact: true })
  await channelSearch.fill('quality-no-match-channel-43e5')
  await expect(screen.getByRole('heading', '没有符合关键词的记录', { exact: true })).toBeVisible()
  await screen.getByRole('button', '清除搜索', { exact: true }).click()
  await expect(channelSearch).toHaveValue('')
  await browser.setViewport({ width: 390, height: 844 })
  await browser.goto('/settings')
  await expect(screen.getByRole('navigation', '标签栏')).toBeVisible()
  await screen.getByRole('button', '深色', { exact: true }).click()
  await expect(browser.locator('html')).toHaveAttribute('data-theme', 'dark')
  await app.screenshot('quality-settings-dark-narrow')
  await browser.goto('/no-such-quality-page')
  await expect(screen.getByRole('heading', '找不到这个页面', { level: 1 })).toBeVisible()
  await app.screenshot('quality-app-not-found')
  await screen.getByRole('link', '返回消息', { exact: true }).click()
  await expect(browser).toHaveURL(/\/$/)
  await expect(screen.getByRole('button', '新 Bot', { exact: true, visible: true })).toBeEnabled()
})
