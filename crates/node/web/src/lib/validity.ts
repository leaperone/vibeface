type Field = HTMLInputElement | HTMLTextAreaElement | HTMLSelectElement

function message(field: Field) {
  const state = field.validity
  if (state.valueMissing) return field instanceof HTMLInputElement && field.type === 'file' ? '还没选文件' : '这一项还没填'
  if (state.tooShort && !(field instanceof HTMLSelectElement)) return `至少 ${field.minLength} 个字符`
  if (state.typeMismatch) return '地址不完整，例如 https://'
  if (state.patternMismatch) return '格式不对'
  return ''
}

/** The browser words its own form checks in its UI language; these say the same in the page's. */
export function installValidityMessages() {
  const isField = (target: EventTarget | null): target is Field => target instanceof HTMLInputElement || target instanceof HTMLTextAreaElement || target instanceof HTMLSelectElement
  const invalid = (event: Event) => { if (isField(event.target)) { event.target.setCustomValidity(''); event.target.setCustomValidity(message(event.target)) } }
  const edited = (event: Event) => { if (isField(event.target)) event.target.setCustomValidity('') }
  document.addEventListener('invalid', invalid, true)
  document.addEventListener('input', edited, true)
  document.addEventListener('change', edited, true)
  return () => {
    document.removeEventListener('invalid', invalid, true)
    document.removeEventListener('input', edited, true)
    document.removeEventListener('change', edited, true)
  }
}
