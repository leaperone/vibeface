import assert from 'node:assert/strict'
import { expect } from 'e2e'
import { test } from '@e2e-dev/web'

test('keeps the guide language, narrow layout and production search clear', async ({ app, screen, browser }) => {
  await browser.setViewport({ width: 320, height: 900 })
  await browser.emulateMedia({ reducedMotion: 'reduce' })
  await app.open()
  await expect(screen.getByText('Guides are currently in Chinese.', { exact: true })).toBeVisible()
  const geometry = await browser.evaluate(() => ({ content: document.documentElement.scrollWidth, available: document.documentElement.clientWidth }))
  assert(geometry.content <= geometry.available, 'The landing page fits the available width including the scrollbar')
  await app.screenshot('quality-landing-320')
  await browser.goto('/docs/quick-start/')
  await browser.setViewport({ width: 390, height: 844 })
  await screen.getByRole('button', '打开文档目录', { visible: true }).click()
  const mobileDirectory = browser.locator('#nd-sidebar-mobile')
  await expect(mobileDirectory).toBeVisible()
  await expect(browser.locator('#nd-sidebar-mobile :focus')).toHaveCount(1)
  const groupOpacity = await browser.evaluate(async () => {
    const sidebar = document.getElementById('nd-sidebar-mobile')
    if (!sidebar) throw new Error('The document directory is missing')
    await Promise.all(sidebar.getAnimations({ subtree: true }).filter(animation => animation.effect?.getComputedTiming().iterations !== Infinity).map(animation => animation.finished.catch(() => {})))
    return [...sidebar.querySelectorAll<HTMLElement>('button')].filter(button => ['开始使用', 'Bot 与任务', '设备与安全', '部署与开发'].includes(button.textContent?.trim() ?? '')).map(button => {
      let element: HTMLElement | null = button, opacity = 1
      while (element) { opacity *= Number(getComputedStyle(element).opacity); element = element.parentElement }
      return { label: button.textContent, opacity }
    })
  })
  assert.equal(groupOpacity.length, 4)
  assert(groupOpacity.every(group => group.opacity >= .99), 'Collapsed directory categories remain visibly opaque with reduced motion')
  const startGroup = mobileDirectory.getByRole('button', '开始使用', { exact: true })
  await expect(startGroup).toHaveAttribute('aria-expanded', 'true')
  const developerGroup = mobileDirectory.getByRole('button', '部署与开发', { exact: true })
  await expect(developerGroup).toHaveAttribute('aria-expanded', 'false')
  await developerGroup.click()
  await expect(developerGroup).toHaveAttribute('aria-expanded', 'true')
  await expect(mobileDirectory.getByRole('link', '构建与发布')).toBeVisible()
  const lastDirectoryLink = mobileDirectory.getByRole('link').last()
  await lastDirectoryLink.focus()
  await browser.keyboard.press('Tab')
  await expect(mobileDirectory.getByRole('button', '切换主题', { exact: true })).toBeFocused()
  await browser.keyboard.press('Shift+Tab')
  await expect(lastDirectoryLink).toBeFocused()
  await app.screenshot('quality-docs-grouped-directory')
  await browser.keyboard.press('Escape')
  await expect(mobileDirectory).toBeHidden()
  await expect(screen.getByRole('button', '打开文档目录', { visible: true })).toBeFocused()
  const search = screen.getByRole('button', '打开文档搜索', { visible: true }).first()
  await search.focus()
  await search.press('Enter')
  await expect(screen.getByRole('dialog')).toBeVisible()
  const spatialAnimations = await browser.evaluate(() => {
    const dialog = document.querySelector('.site-search-dialog')
    if (!dialog) throw new Error('The document search dialog is missing')
    return dialog.getAnimations().filter(animation => {
      if (!(animation.effect instanceof KeyframeEffect)) return false
      const positions = animation.effect.getKeyframes().map(frame => [frame.transform, frame.translate, frame.scale, frame.rotate].join('|'))
      return new Set(positions).size > 1
    }).map(animation => animation instanceof CSSAnimation ? animation.animationName : animation.id)
  })
  assert.deepEqual(spatialAnimations, [], 'Reduced-motion search opens without a spatial animation')
  await screen.getByPlaceholder('搜索文档').fill('恢复')
  await expect(screen.getByRole('option').first()).toBeVisible()
  const results = await browser.evaluate(() => [...document.querySelectorAll<HTMLElement>('[role="option"]')].map(element => ({
    height: element.getBoundingClientRect().height,
    url: element.id.replace('fd-search-option-', ''),
  })))
  assert(results.length > 1 && results.every(result => result.height <= 160), 'All result excerpts leave room to compare matches')
  await expect(screen.getByRole('option').filter({ hasText: '日常使用指南' })).toBeVisible()
  await app.screenshot('quality-docs-search-results')
  await screen.getByPlaceholder('搜索文档').fill('no-results-quality-check-9e2b')
  await expect(screen.getByRole('status').filter({ hasText: '试试更短的关键词' })).toBeVisible()
  await app.screenshot('quality-docs-no-results-recovery')
  await browser.keyboard.press('Escape')
  await expect(screen.getByRole('dialog')).toHaveCount(0)
  await expect(search).toBeFocused()
  await browser.keyboard.press('Control+k')
  await expect(screen.getByRole('dialog')).toBeVisible()
  await browser.keyboard.press('Escape')
  await expect(screen.getByRole('dialog')).toHaveCount(0)
  await screen.getByRole('heading', /^从源码准备当前版本/).getByRole('link', '从源码准备当前版本', { exact: true }).click()
  await expect(screen.getByText('git clone https://github.com/leaperone/vibeface.git', { exact: false })).toBeVisible()
  await app.screenshot('quality-docs-source-guide')
})

test('gives an actionable search-index failure without discarding navigation', async ({ app, screen, browser }) => {
  await browser.emulateMedia({ reducedMotion: 'no-preference' })
  const indexRoute = /\/search-index\.json(?:\?|$)/
  await browser.route(indexRoute, route => route.fulfill({ status: 503, json: { error: 'temporary-fixture-failure' } }))
  await app.open('/docs/quick-start/')
  const opener = screen.getByRole('button', '打开文档搜索', { visible: true }).first()
  await opener.click()
  await screen.getByPlaceholder('搜索文档').fill('恢复')
  await expect(screen.getByRole('alert').filter({ hasText: '暂时无法搜索文档' })).toBeVisible()
  await expect(screen.getByRole('button', '重试搜索', { exact: true })).toBeVisible()
  await app.screenshot('quality-docs-search-failure')
  await browser.unroute(indexRoute)
  await screen.getByRole('button', '重试搜索', { exact: true }).click()
  await expect(screen.getByPlaceholder('搜索文档')).toHaveValue('恢复')
  await expect(screen.getByPlaceholder('搜索文档')).toBeFocused()
  await expect(screen.getByRole('option').first()).toBeVisible()
  await expect(screen.getByRole('alert')).toHaveCount(0)
  await app.screenshot('quality-docs-search-retry-preserves-query')
  await browser.keyboard.press('Escape')
  await expect(screen.getByRole('dialog')).toHaveCount(0)
  await expect(opener).toBeFocused()
  await expect(screen.getByRole('main')).toBeVisible()
})

test('keeps keyboard focus distinct in dark reduced-motion documentation', async ({ app, screen, browser }) => {
  await browser.setViewport({ width: 390, height: 844 })
  await browser.emulateMedia({ colorScheme: 'dark', reducedMotion: 'reduce' })
  await app.open('/docs/quick-start/')
  await expect(browser.locator('html')).toHaveAttribute('class', /dark/)
  const indicator = () => browser.evaluate(() => {
    const active = document.activeElement
    if (!(active instanceof HTMLElement)) throw new Error('The keyboard control is missing')
    const canvas = document.createElement('canvas'), paint = canvas.getContext('2d', { willReadFrequently: true })
    if (!paint) throw new Error('Canvas color conversion is unavailable')
    const rgba = (value: string) => { paint.clearRect(0, 0, 1, 1); paint.fillStyle = value; paint.fillRect(0, 0, 1, 1); return [...paint.getImageData(0, 0, 1, 1).data] }
    const luminance = (rgb: number[]) => rgb.slice(0, 3).map(value => value / 255).map(value => value <= .04045 ? value / 12.92 : ((value + .055) / 1.055) ** 2.4).reduce((sum, value, index) => sum + value * [.2126, .7152, .0722][index], 0)
    const style = getComputedStyle(active)
    let parent: HTMLElement | null = active, background = [0, 0, 0, 0]
    while (parent && background[3] !== 255) { background = rgba(getComputedStyle(parent).backgroundColor); parent = parent.parentElement }
    if (background[3] !== 255) throw new Error('The focus background is not observable')
    const foreground = luminance(rgba(style.outlineColor)), backdrop = luminance(background)
    return { width: parseFloat(style.outlineWidth), style: style.outlineStyle, contrast: (Math.max(foreground, backdrop) + .05) / (Math.min(foreground, backdrop) + .05), background: backdrop }
  })
  const opener = screen.getByRole('button', '打开文档目录', { visible: true })
  await opener.focus()
  await opener.press('Enter')
  await expect(browser.locator('#nd-sidebar-mobile :focus')).toHaveCount(1)
  for (let index = 0; index < 2; index++) {
    const focus = await indicator()
    assert(focus.background < .1, 'This check observes an actual dark surface')
    assert(focus.width >= 2 && focus.style !== 'none', 'The focused control has a clear outline')
    assert(focus.contrast >= 3, 'The focus indicator contrasts with its adjacent surface')
    await browser.keyboard.press('Tab')
  }
  await app.screenshot('quality-docs-dark-focus')
  await browser.keyboard.press('Escape')
  await expect(browser.locator('#nd-sidebar-mobile')).toBeHidden()
  await expect(opener).toBeFocused()
  await browser.goto('/')
  const docsLink = screen.getByRole('link', /^Docs/).first()
  await docsLink.focus()
  await expect(docsLink).toBeFocused()
  const homeFocus = await indicator()
  assert(homeFocus.width >= 2 && homeFocus.contrast >= 3, 'Marketing navigation uses the same visible keyboard focus')
})
