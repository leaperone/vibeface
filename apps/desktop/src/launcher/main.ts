import type { LauncherApi, LauncherStatus } from '../shared/contract.js'
import { describe, type ActionId, type View } from './view.js'

declare global {
  interface Window {
    /** Exposed by the trusted launcher preload; absent in any other window. */
    vibefaceLauncher: LauncherApi
  }
}

const RELEASES = 'https://github.com/leaperone/vibeface/releases'

const api = window.vibefaceLauncher

function element<T extends HTMLElement>(id: string, kind: new () => T): T {
  const found = document.getElementById(id)
  if (!(found instanceof kind)) throw new Error(`launcher markup is missing #${id}`)
  return found
}

const scope = element('scope', HTMLParagraphElement)
const indicator = element('indicator', HTMLSpanElement)
const indicatorText = element('indicator-text', HTMLSpanElement)
const card = element('card', HTMLElement)
const headline = element('headline', HTMLHeadingElement)
const explain = element('explain', HTMLParagraphElement)
const facts = element('facts', HTMLDListElement)
const diagnostics = element('diagnostics', HTMLDetailsElement)
const failure = element('failure', HTMLParagraphElement)
const actions = element('actions', HTMLDivElement)
const nodes = element('nodes', HTMLUListElement)
const discoveryMessage = element('discovery-message', HTMLParagraphElement)
const backgroundNode = element('background-node', HTMLElement)
const backgroundMessage = element('background-message', HTMLParagraphElement)
const backgroundActions = element('background-actions', HTMLDivElement)
const autoOpen = element('auto-open', HTMLInputElement)
const profilePath = element('profile-dir', HTMLElement)
const corePath = element('core-path', HTMLElement)
const version = element('version', HTMLElement)
const forget = element('forget', HTMLButtonElement)
const releases = element('releases', HTMLButtonElement)

let working = false
let lastHeadline = ''

function run(work: () => Promise<LauncherStatus | void>): void {
  if (working) return
  working = true
  setBusy(true)
  void work()
    .then((status) => {
      if (status) paint(status)
    })
    .catch((error: unknown) => {
      failure.textContent = error instanceof Error ? error.message : '出现未知错误。'
      failure.hidden = false
    })
    .finally(() => {
      working = false
      setBusy(false)
    })
}

function setBusy(busy: boolean): void {
  for (const control of document.querySelectorAll<HTMLButtonElement>('#actions button, #nodes button, #background-actions button')) {
    control.disabled = busy || control.dataset['locked'] === 'true'
  }
  forget.disabled = busy
  autoOpen.disabled = busy
}

const RUN: Record<ActionId, () => Promise<LauncherStatus>> = {
  controller: () => api.chooseNodeMode('controller'),
  local: () => api.chooseNodeMode('local'),
  discover: () => api.discoverNodes(),
  'start-owned': () => api.startOwned(),
  'stop-owned': () => api.stopOwned(),
  open: () => api.openBusiness(),
  choose: () => api.chooseDataDir(),
  connect: () => api.getStatus(),
  'start-discovered': () => api.getStatus(),
  'install-background': () => api.installBackgroundNode(),
  'start-background': () => api.startBackgroundNode(),
  'stop-background': () => api.stopBackgroundNode(),
  'remove-background': () => api.removeBackgroundNode(),
}

function paintActions(target: HTMLDivElement, items: View['actions']): void {
  target.replaceChildren()
  for (const item of items) {
    const button = document.createElement('button')
    button.type = 'button'
    button.dataset['action'] = item.id
    button.textContent = item.label
    button.disabled = item.disabled ?? false
    button.dataset['locked'] = item.disabled ? 'true' : 'false'
    if (item.primary) button.classList.add('primary')
    button.addEventListener('click', () => run(() => {
      if (item.id === 'connect') return api.connectDiscoveredNode(item.nodeId ?? '')
      if (item.id === 'start-discovered') return api.startDiscoveredNode(item.nodeId ?? '')
      return RUN[item.id]()
    }))
    target.append(button)
  }
}

function paintNodes(view: View): void {
  nodes.replaceChildren()
  nodes.hidden = view.rows.length === 0
  for (const row of view.rows) {
    const item = document.createElement('li')
    const content = document.createElement('div')
    content.className = 'node-copy'
    const title = document.createElement('strong')
    title.textContent = `${row.status} · ${row.node.deviceId.slice(0, 12)}`
    const detail = document.createElement('span')
    detail.textContent = row.detail
    const location = document.createElement('details')
    location.className = 'node-location'
    const summary = document.createElement('summary')
    summary.textContent = '节点位置'
    const path = document.createElement('span')
    path.textContent = row.node.dataDir
    path.className = 'path'
    location.append(summary, path)
    content.append(title, detail, location)
    const controls = document.createElement('div')
    controls.className = 'node-controls'
    paintActions(controls, row.actions)
    item.append(content, controls)
    nodes.append(item)
  }
}

function paintFacts(view: View): void {
  diagnostics.hidden = view.facts.length === 0
  facts.replaceChildren()
  for (const [label, value] of view.facts) {
    const term = document.createElement('dt')
    term.textContent = label
    const detail = document.createElement('dd')
    detail.textContent = value
    if (label.includes('目录') || label.includes('路径') || label.includes('入口')) detail.classList.add('path')
    facts.append(term, detail)
  }
}

function paint(status: LauncherStatus): void {
  const view = describe(status)
  scope.textContent = view.scope
  indicator.dataset['tone'] = view.tone
  indicatorText.textContent = view.indicator
  headline.textContent = view.headline
  explain.textContent = view.explain
  failure.textContent = view.failure ?? ''
  failure.hidden = view.failure === null
  paintFacts(view)
  paintActions(actions, view.actions)
  paintNodes(view)
  discoveryMessage.textContent = view.discoveryMessage
  backgroundNode.hidden = view.backgroundActions.length === 0 && !view.backgroundMessage
  backgroundMessage.textContent = view.backgroundMessage
  paintActions(backgroundActions, view.backgroundActions)

  if (view.headline !== lastHeadline) {
    lastHeadline = view.headline
    card.removeAttribute('data-entering')
    void card.offsetWidth
    card.dataset['entering'] = 'true'
    card.addEventListener('animationend', () => card.removeAttribute('data-entering'), { once: true })
  }

  autoOpen.checked = status.preferences.openBusinessOnConnect
  profilePath.textContent = status.profileDir
  corePath.textContent = status.runtime.corePath
  version.textContent = status.version
  setBusy(working)
}

autoOpen.addEventListener('change', () => {
  const next = autoOpen.checked
  run(() => api.setPreference('openBusinessOnConnect', next))
})

forget.addEventListener('click', () => run(() => api.forgetDataDir()))

releases.addEventListener('click', () => void api.openExternal(RELEASES))

api.onStatus(paint)
run(() => api.getStatus())
