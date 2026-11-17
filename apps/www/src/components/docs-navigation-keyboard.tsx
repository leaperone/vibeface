import { useEffect, useRef } from 'react'
import { useSidebar } from 'fumadocs-ui/layouts/docs/slots/sidebar'
import { useSearchContext } from 'fumadocs-ui/contexts/search'

const controls = 'a[href], button, input, select, textarea, [tabindex]'

function visible(element: HTMLElement) {
  return element.getClientRects().length > 0
    && getComputedStyle(element).visibility !== 'hidden'
    && !element.closest('[hidden], [inert], [aria-hidden="true"]')
}

/** Fumadocs 16 renders its mobile drawer without modal keyboard behavior. */
export function DocsNavigationKeyboard() {
  const { open, mode, setOpen } = useSidebar()
  const { open: searchOpen } = useSearchContext()
  const searchIsOpen = useRef(searchOpen)
  useEffect(() => { searchIsOpen.current = searchOpen }, [searchOpen])

  useEffect(() => {
    if (!open || mode !== 'drawer') return
    const element = document.getElementById('nd-sidebar-mobile')
    if (!element) return
    const drawer = element
    const openedAt = window.location.href
    const active = document.activeElement
    const opener = active instanceof HTMLElement && active.matches('[aria-controls="nd-sidebar-mobile"]') && !drawer.contains(active)
      ? active
      : [...document.querySelectorAll<HTMLElement>('[aria-controls="nd-sidebar-mobile"]')].find(element => !drawer.contains(element) && visible(element))
    const attributes = { role: 'dialog', 'aria-modal': 'true', 'aria-label': '文档目录', tabindex: '-1' }
    const previous = Object.keys(attributes).map(name => [name, drawer.getAttribute(name)] as const)
    for (const [name, value] of Object.entries(attributes)) drawer.setAttribute(name, value)

    function otherOverlay() {
      return searchIsOpen.current || [...document.querySelectorAll<HTMLElement>('[role="dialog"], [role="alertdialog"], [role="menu"], [role="listbox"], dialog[open]')]
        .some(element => element !== drawer && visible(element))
    }
    function focusable() {
      return [...drawer.querySelectorAll<HTMLElement>(controls)]
        .filter(element => element.tabIndex >= 0 && !element.matches(':disabled, [aria-disabled="true"]') && visible(element))
    }
    function focusDirectory() {
      const current = drawer.querySelector<HTMLElement>('a[data-active="true"]')
      const target = current && visible(current) ? current : focusable()[0] ?? drawer
      target.focus({ preventScroll: true })
    }
    function onKeyDown(event: KeyboardEvent) {
      if (event.defaultPrevented || otherOverlay()) return
      if (event.key === 'Escape') {
        event.preventDefault()
        event.stopPropagation()
        setOpen(false)
      } else if (event.key === 'Tab') {
        const items = focusable()
        const first = items[0] ?? drawer
        const last = items.at(-1) ?? drawer
        const focused = document.activeElement
        if (!drawer.contains(focused) || focused === drawer || (event.shiftKey ? focused === first : focused === last)) {
          event.preventDefault()
          const target = event.shiftKey ? last : first
          target.focus({ preventScroll: true })
        }
      }
    }
    function onFocusIn(event: FocusEvent) {
      if (!drawer.contains(event.target instanceof Node ? event.target : null) && !otherOverlay()) focusDirectory()
    }

    if (!otherOverlay()) focusDirectory()
    document.addEventListener('keydown', onKeyDown)
    document.addEventListener('focusin', onFocusIn)
    return () => {
      document.removeEventListener('keydown', onKeyDown)
      document.removeEventListener('focusin', onFocusIn)
      for (const [name, value] of previous) {
        if (value === null) drawer.removeAttribute(name)
        else drawer.setAttribute(name, value)
      }
      // Route changes own their destination focus; temporary overlays own theirs.
      if (window.location.href === openedAt && opener?.isConnected && !otherOverlay()) opener.focus({ preventScroll: true })
    }
  }, [open, mode, setOpen])

  return null
}
