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
const failure = element('failure', HTMLParagraphElement)
const actions = element('actions', HTMLDivElement)
const autoOpen = element('auto-open', HTMLInputElement)
const profilePath = element('profile-dir', HTMLElement)
const corePath = element('core-path', HTMLElement)
const version = element('version', HTMLElement)
const forget = element('forget', HTMLButtonElement)
const releases = element('releases', HTMLAnchorElement)

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
  for (const control of actions.querySelectorAll('button')) control.disabled = busy || control.dataset['locked'] === 'true'
  forget.disabled = busy
  autoOpen.disabled = busy
}

const RUN: Record<ActionId, () => Promise<LauncherStatus>> = {
  choose: () => api.chooseDataDir(),
  start: () => api.startOwned(),
  attach: () => api.attachExisting(),
  stop: () => api.stopOwned(),
  open: () => api.openBusiness(),
  recheck: () => api.getStatus(),
}

function paintActions(view: View): void {
  actions.replaceChildren()
  for (const item of view.actions) {
    const button = document.createElement('button')
    button.type = 'button'
    button.textContent = item.label
    button.disabled = item.disabled
    button.dataset['locked'] = item.disabled ? 'true' : 'false'
    if (item.primary) button.classList.add('primary')
    button.addEventListener('click', () => run(RUN[item.id]))
    actions.append(button)
  }
}

function paintFacts(view: View): void {
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
  paintActions(view)

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

releases.addEventListener('click', (event) => {
  event.preventDefault()
  void api.openExternal(RELEASES)
})

api.onStatus(paint)
run(() => api.getStatus())
