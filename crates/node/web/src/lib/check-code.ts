/**
 * Interim 12-digit check code shown on both sides of a device join (docs/design/web-ux.md section 4.3).
 * ponytail: a compressed view of ids both sides already hold, about 40 bits; replaced by the 6-digit
 * committed safety code in unified design phase 2.
 */
export async function checkCode(networkId: string, inviterId: string, newDeviceId: string): Promise<string> {
  const bytes = new Uint8Array(await crypto.subtle.digest("SHA-256", new TextEncoder().encode(`vibeface-check-v0\n${networkId}\n${inviterId}\n${newDeviceId}`)))
  const value = new DataView(bytes.buffer).getBigUint64(0)
  return (value % 1_000_000_000_000n).toString().padStart(12, "0")
}
