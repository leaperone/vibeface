const encoder = new TextEncoder()

export function passphrase(password: string, confirmation: string): string {
  if ([...password].length < 12 || encoder.encode(password).length > 1024) throw new Error('恢复包口令需要至少 12 个字符，且不超过 1024 字节。')
  if (password !== confirmation) throw new Error('两次口令不一致。')
  return password
}
export function downloadPackage(bytes: Uint8Array, networkId: string) {
  const copy = new Uint8Array(bytes)
  const url = URL.createObjectURL(new Blob([copy.buffer], { type: 'application/json' }))
  const link = document.createElement('a')
  link.href = url
  link.download = `vibeface-recovery-${networkId.slice(0, 12)}.json`
  link.click()
  setTimeout(() => URL.revokeObjectURL(url), 1000)
}
