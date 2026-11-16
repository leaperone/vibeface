/** Encodes independent blocks of complete base64 triplets without a file-sized binary string. */
export function bytesToBase64(bytes: Uint8Array): string {
  const blocks: string[] = []
  const blockBytes = 3 * 8192
  for (let offset = 0; offset < bytes.length; offset += blockBytes) {
    blocks.push(btoa(String.fromCharCode(...bytes.subarray(offset, offset + blockBytes))))
  }
  return blocks.join('')
}

/** Uses the runtime's base64 decoder; domain boundaries decide whether canonical input is required. */
export function base64ToBytes(value: string): Uint8Array<ArrayBuffer> {
  return Uint8Array.from(atob(value), char => char.charCodeAt(0))
}

export function bytesToHex(bytes: Uint8Array): string {
  return [...bytes].map(byte => byte.toString(16).padStart(2, '0')).join('')
}

export async function sha256Hex(bytes: Uint8Array): Promise<string> {
  return bytesToHex(new Uint8Array(await crypto.subtle.digest('SHA-256', new Uint8Array(bytes))))
}
