
import assert from "node:assert/strict"
import test from "node:test"


import type { HistorySection } from "@vibeface/client/workspace/types.ts"
import { parseConversation } from "@vibeface/client/workspace/model.ts"
import { messageTimes } from "@vibeface/client/chat/timeline.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"


import { context, componentHarness, componentText, parityShared, settleComponent } from './entry-url-test-kit.test-support.ts'


test('Web and Mobile expose every omitted section, fetch only on demand and offer cancellation', async () => {
  for (const platform of ['web', 'mobile']) {
    const calls: string[] = [], cancellations: string[] = []
    const shared = { ...parityShared(), data: parseConversation({ ...context, history: Object.fromEntries(workspaceModel.historySections.map((section) => [section, { next_cursor: `${section}-cursor`, complete: false }])) }),
      loadHistory: async (section: HistorySection) => { calls.push(section) }, cancelHistory: (section: HistorySection) => { cancellations.push(section) },
    }
    const controls = componentHarness(platform === 'web' ? '../chat/history-controls.tsx' : '../../../../../apps/mobile/src/features/chat/history-controls.tsx', {})
    const render = () => controls.render('HistoryControls', { workspace: shared })
    const buttons = () => render().filter((element) => element.type === 'Button')
    const press = (button: ReturnType<typeof buttons>[number]) => {
      const action = platform === 'web' ? button.props.onClick : button.props.onPress
      assert.equal(typeof action, 'function')
      if (typeof action === 'function') action()
    }
    try {
      assert.equal(buttons().length, 19)
      assert.deepEqual(calls, [], 'opening a bounded snapshot never fetches all history')
      const messages = buttons().find((button) => componentText(button.props.children) === '读取更多聊天消息')
      assert.ok(messages); press(messages); await settleComponent()
      assert.deepEqual(calls, ['messages'])
      shared.historyLoading = new Set<HistorySection>(['messages'])
      const cancel = buttons().find((button) => componentText(button.props.children) === '取消读取聊天消息')
      assert.ok(cancel); press(cancel)
      assert.deepEqual(cancellations, ['messages'])
      shared.connectionOnline = false
      assert(buttons().filter((button) => componentText(button.props.children).startsWith('读取更多')).every((button) => button.props.disabled === true))
      shared.data.history.messages = { complete: true, next_cursor: null }
      assert.equal(buttons().length, 18)
    } finally { controls.close() }
  }
})


test('the Web chat keeps the first visible identity and offset through older pages, pushes and streaming updates', () => {
  const row = (key: string) => ({ key, message: { message_id: key, role: 'user', text: key, event_hash: key, created_at_ms: 1 } })
  let positions = [{ key: 'one', top: 0 }, { key: 'two', top: 200 }, { key: 'three', top: 400 }]
  const view = { scrollTop: 200, scrollHeight: 1000, clientHeight: 200, getBoundingClientRect: () => ({ top: 0 }),
    querySelectorAll: () => positions.map((item) => ({ dataset: { historyKey: item.key }, getBoundingClientRect: () => ({ top: item.top - view.scrollTop, bottom: item.top + 100 - view.scrollTop }) })), querySelector: () => ({}),
  }
  const chat = componentHarness('../../components/arc/conversation-thread.tsx', { '@vibeface/client/chat/timeline.ts': { messageTimes }, '@/lib/utils': { cn: (...values: unknown[]) => values.filter(Boolean).join(' ') } }, { ResizeObserver: class { observe() {} disconnect() {} }, matchMedia: () => ({ matches: true }) })
  const render = (keys: string[]) => chat.render('Chat', { entries: keys.map(row), empty: null })
  try {
    const viewport = render(['one', 'two', 'three']).find((element) => element.type === 'div' && element.props.role === 'log')
    assert.ok(viewport)
    workspaceModel.record(viewport.props.ref).current = view
    assert.equal(typeof viewport.props.onScroll, 'function')
    if (typeof viewport.props.onScroll === 'function') viewport.props.onScroll()
    positions = [{ key: 'older', top: 0 }, { key: 'one', top: 150 }, { key: 'two', top: 350 }, { key: 'three', top: 550 }]
    let elements = render(['older', 'one', 'two', 'three'])
    assert.equal(view.scrollTop, 350)
    assert.equal(elements.some((element) => element.type === 'Button' && element.props['aria-label'] === '回到最新消息'), true)
    positions.push({ key: 'pushed', top: 750 }); view.scrollHeight = 1200
    render(['older', 'one', 'two', 'three', 'pushed'])
    assert.equal(view.scrollTop, 350)
    positions = positions.map((item) => item.key === 'two' || item.key === 'three' || item.key === 'pushed' ? { ...item, top: item.top + 40 } : item)
    elements = render(['older', 'one', 'two', 'three', 'pushed'])
    assert.equal(view.scrollTop, 390)
    assert.equal(elements.some((element) => element.type === 'Button' && element.props['aria-label'] === '回到最新消息'), true)
  } finally { chat.close() }
})
