import type { GenerationAction } from '@vibeface/client/lib/use-generation-action.ts'
import assert from "node:assert/strict"
import test from "node:test"



import { formatSize } from "@vibeface/client/workspace/model.ts"
import * as workspaceModel from "@vibeface/client/workspace/model.ts"


import { historyActions, componentHarness, pendingResult, settleComponent } from './entry-url-test-kit.test-support.ts'


test("artifact reads retain their offset while old responses cannot append or unlock newer reads", async () => {
  let generation = 1, work = pendingResult()
  const offsets: number[] = []
  const workspace = () => { const owner = generation; return { ...historyActions, generation: owner, isCurrent: () => owner === generation, connectionOnline: true,
    readArtifact: (_hash: string, offset: number) => { offsets.push(offset); return work.promise }, run: async (action: () => Promise<void>) => { try { await action() } catch {} } } }
  let currentWorkspace = workspace()
  const app = componentHarness('../chat/elements/artifact.tsx', { '@vibeface/client/workspace/context.tsx': { useWorkspace: () => currentWorkspace }, '@vibeface/client/workspace/model.ts': { formatSize } })
  const render = () => app.render('Artifact', { file: { hash: 'result', size: 5, name: 'notes.txt' } })
  const open = () => render().find(({ props }) => props.label === '打开')?.props
  try {
    open()?.action?.(); assert.equal(open()?.disabled, true)
    const old = work
    generation++; currentWorkspace = workspace()
    assert.equal(open()?.disabled, false)
    work = pendingResult(); open()?.action?.()
    old.resolve({ size: 5, next_offset: null, text: 'stale' }); await settleComponent()
    assert.equal(open()?.disabled, true)
    work.resolve({ size: 5, next_offset: null, text: 'fresh' }); await settleComponent()
    assert.equal(render().find(({ type }) => type === 'pre')?.props.children, 'fresh')
    assert.deepEqual(offsets, [0, 0])
    assert.ok(render().some(({ type, props }) => type === 'span' && props.title === 'notes.txt' && (props.children as { props?: { name?: string } } | undefined)?.props?.name === 'notes.txt'), 'the card names the file')
  } finally { app.close() }
})


test("a long message of the user's folds behind an icon button; a short one does not", () => {
  const app = componentHarness('../chat/elements/message.tsx', {})
  const toggle = (text: string) => app.render('FoldedText', { text }).find(({ type }) => type === 'Button')?.props
  assert.equal(toggle('短消息'), undefined)
  const lines = (count: number) => Array(count).fill('一行').join('\n')
  assert.equal(toggle(lines(12)), undefined)
  for (const text of [lines(13), 'x'.repeat(601)]) {
    assert.equal(toggle(text)?.['aria-expanded'], false)
    assert.ok(app.render('FoldedText', { text }).some(({ props }) => props.className === 'line-clamp-8'))
    toggle(text)?.onClick?.()
    assert.equal(toggle(text)?.['aria-expanded'], true)
    assert.equal(toggle(text)?.['aria-label'], '收起')
    assert.ok(!app.render('FoldedText', { text }).some(({ props }) => props.className === 'line-clamp-8'))
    toggle(text)?.onClick?.()
  }
})


test("a toast action invokes its callback and dismisses the corresponding background notice", () => {
  const app = componentHarness('../../components/arc/product-kit.tsx', { '@/lib/utils': { cn: (...names: unknown[]) => names.filter(Boolean).join(' ') } })
  let opened = 0
  try {
    ;(app.module.toast as (text: string, options: object) => void)('已发出', { persist: true, action: { label: '查看', onClick() { opened++ } } })
    const action = app.render('Toaster').find(({ type, props }) => type === 'Button' && props.children === '查看')
    assert.ok(action)
    action.props.onClick?.()
    assert.equal(opened, 1)
    assert.equal(app.render('Toaster').some(({ props }) => props.children === '查看'), false)
  } finally { app.close() }
})


test("Arc segmented control keeps one Tab stop and arrow navigation; product saving gates changes", () => {
  const options = ['light','dark','system'].map(value => ({ value,label:value }))
  let value = 'dark', focused = ''
  const app = componentHarness('../../../../../packages/uiarc/src/segmented-control/segmented-control.tsx', { 'motion/react': { LayoutGroup: 'LayoutGroup', motion: { div: 'div', span: 'span' }, useReducedMotion: () => true }, '../lib/motion-tokens': { motionTokens: {} } }, { CSS: { escape: (text: string) => text } })
  const render = () => app.render('default', { label: '外观', value, options, onValueChange: (next: string) => { value = next } })
  try {
    const track = render().find(({ props }) => props.ref)
    assert.ok(track)
    workspaceModel.record(track.props.ref).current = { querySelector: (selector: string) => ({ focus: () => { focused = selector } }) }
    const press = (key: string) => { let prevented = false; const button = render().find(({ props }) => props['aria-pressed'] === true) ?? render().find(({ props }) => props['data-value']); (button?.props.onKeyDown as (event: unknown) => void)({ key,preventDefault: () => { prevented = true } }); return prevented }
    assert.deepEqual(render().filter(({ props }) => props['data-value']).map(({ props }) => props.tabIndex),[-1,0,-1])
    assert.ok(press('ArrowRight')); assert.equal(value,'system'); assert.match(focused,/system/)
    press('ArrowRight'); assert.equal(value,'light')
    press('ArrowLeft'); assert.equal(value,'system')
    press('Home'); assert.equal(value,'light')
    press('End'); assert.equal(value,'system')
    assert.equal(press('Tab'),false)
    value = 'unknown'
    assert.deepEqual(render().filter(({ props }) => props['data-value']).map(({ props }) => props.tabIndex),[0,-1,-1])
  } finally { app.close() }
  const product = componentHarness('../../components/arc/product-kit.tsx', {})
  try {
    const segment = product.render('Segmented', { label: '外观',value:'dark',options,disabled:true,onChange: () => { throw new Error('saving must not submit another change') } }).find(({ props }) => props.onValueChange)
    assert.ok(segment)
    ;(segment.props.onValueChange as (next: string) => void)('light')
  } finally { product.close() }
})


test('recovery restore clears submitted passwords before waiting and keeps the chosen file and name', async () => {
  let generation = 1, work = pendingResult()
  class Input { value = 'test-only-value'; files: object[] = [{}]; checked = false }
  const fields = new Map(['bundle', 'password', 'old_password', 'confirmation', 'name', 'bootstrap', 'address', 'device_id', 'relay'].map(name => [name, new Input()]))
  const passwords = ['password', 'old_password', 'confirmation'].map(name => fields.get(name))
  const form = { elements: { namedItem: (name: string) => fields.get(name) }, querySelectorAll: () => passwords }
  const device = { connectionGeneration: () => generation, ready: null, review: null, entry: null,
    beginRecovery: (_input: unknown, rebind: GenerationAction['rebind']) => { rebind({ from: generation, to: ++generation }); return work.promise }, replaceRecoveryFile: () => work.promise }
  const app = componentHarness('../identity/recovery-controls.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device }, '@vibeface/client/lib/node.ts': { parseEntry: (value: unknown) => value, messageOf: (cause: Error) => cause.message },
    '@vibeface/client/lib/secure.ts': { passphrase: (value: string) => value }, '@/lib/device-name': { defaultBrowserName: () => 'browser' },
  }, { HTMLInputElement: Input, HTMLTextAreaElement: Input })
  const render = () => app.render('RecoveryControls', { only: 'restore' })
  const submit = () => render().find(({ type }) => type === 'form')?.props.onSubmit?.({ preventDefault() {}, currentTarget: form })
  try {
    render(); render(); submit()
    assert.ok(passwords.every(field => field?.value === ''))
    assert.equal(fields.get('name')?.value, 'test-only-value')
    assert.equal(fields.get('bundle')?.files.length, 1)
    const old = work
    generation++; render()
    const password = fields.get('password'); assert.ok(password); password.value = 'replacement test-only-value'
    old.reject(new Error('stale package failure')); await settleComponent()
    assert.equal(password.value, 'replacement test-only-value')
    assert.equal(render().some(({ props }) => props.error), false)
    work = pendingResult(); submit(); assert.equal(password.value, '')
    work.resolve(undefined); await settleComponent()
    assert.equal(render().find(({ type }) => type === 'fieldset')?.props.disabled, false)
    const bundle = fields.get('bundle'); assert.ok(bundle); bundle.files = []
    password.value = 'another test-only-value'
    submit(); await settleComponent()
    assert.equal(password.value, '')
    assert.equal(render().find(({ props }) => props.error)?.props.error, '选一个恢复文件。')
  } finally { app.close() }
})


test('changing the recovery password keeps what was typed when it fails, and clears it once the new file is made', async () => {
  let work = pendingResult()
  class Input { value = 'test-only-value'; files: object[] = [{}]; checked = false }
  const fields = new Map(['bundle', 'password', 'old_password'].map(name => [name, new Input()]))
  const passwords = ['password', 'old_password'].map(name => fields.get(name))
  const form = { elements: { namedItem: (name: string) => fields.get(name) }, querySelectorAll: () => passwords }
  const device = { connectionGeneration: () => 1, ready: null, review: null, entry: null, replaceRecoveryFile: () => work.promise }
  const app = componentHarness('../identity/recovery-controls.tsx', {
    '@vibeface/client/identity/context.tsx': { useDevice: () => device }, '@vibeface/client/lib/node.ts': { parseEntry: (value: unknown) => value, messageOf: (cause: Error) => cause.message },
    '@vibeface/client/lib/secure.ts': { passphrase: (value: string) => value, passwordStrength: () => ({ level: 'strong' }) }, '@/lib/device-name': { defaultBrowserName: () => 'browser' },
  }, { HTMLInputElement: Input, HTMLTextAreaElement: Input })
  const render = () => app.render('RecoveryControls', { only: 'passphrase' })
  const form_ = () => render().find(({ type }) => type === 'form')?.props
  const submit = () => form_()?.onSubmit?.({ preventDefault() {}, currentTarget: form })
  try {
    render(); submit()
    work.reject(new Error('旧密码不对')); await settleComponent()
    assert.ok(passwords.every(field => field?.value === 'test-only-value'), 'a failed try keeps both passwords')
    assert.equal(render().find(({ props }) => props.error)?.props.error, '旧密码不对')
    form_()?.onChange?.({})
    assert.equal(render().some(({ props }) => props.error), false, 'a change takes the old error away')
    work = pendingResult(); submit(); work.resolve(undefined); await settleComponent()
    assert.ok(passwords.every(field => field?.value === ''), 'a new file clears both passwords')
  } finally { app.close() }
})
