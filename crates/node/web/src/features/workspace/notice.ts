let handoff = ""

export function publishNotice(text: string) {
  handoff = text
}

export function consumeNotice() {
  const text = handoff
  handoff = ""
  return text
}
