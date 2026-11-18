const units = ["字节", "KB", "MB", "GB"]

/** Decimal file sizes. Use floor for a limit so the label never promises more bytes than fit. */
export function formatByteSize(bytes: number, { rounding = "nearest" }: { rounding?: "nearest" | "floor" } = {}): string {
  let unit = 0
  let divisor = 1
  while (unit < units.length - 1 && bytes >= divisor * 1000) {
    unit += 1
    divisor *= 1000
  }
  if (unit === 0) return `${bytes} ${units[unit]}`

  const value = bytes / divisor
  const digits = value < 10 ? 1 : 0
  const scale = 10 ** digits
  const displayed = rounding === "floor" ? Math.floor(bytes / (divisor / scale)) / scale : value
  return `${displayed.toFixed(digits)} ${units[unit]}`
}
