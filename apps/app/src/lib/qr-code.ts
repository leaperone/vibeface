import qrcode from "qrcode-generator"

export type QrCodeResult =
  | { kind: "encoded"; image: string }
  | { kind: "empty" | "too-long" | "failed" }

export function encodeQrCode(link: string): QrCodeResult {
  if (!link) return { kind: "empty" }
  try {
    const code = qrcode(0, "M")
    code.addData(link)
    code.make()
    return { kind: "encoded", image: code.createDataURL(4, 16) }
  } catch (cause) {
    return { kind: typeof cause === "string" && cause.startsWith("code length overflow.") ? "too-long" : "failed" }
  }
}
