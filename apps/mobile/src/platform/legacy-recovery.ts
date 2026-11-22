import type { File } from 'expo-file-system'
import { record } from '@vibeface/client/lib/node.ts'

/** The sealed recovery copies written directly in the app's private Documents directory by earlier versions. */
type LegacyRecoveryFile = Pick<File, 'name' | 'size' | 'textSync'>

export function isLegacyRecoveryFile(file: LegacyRecoveryFile): boolean {
  if (!/^vibeface-recovery-[A-F0-9]{8}-\d{4}-(0[1-9]|1[0-2])-(0[1-9]|[12]\d|3[01])\.json$/.test(file.name) || file.size < 1 || file.size > 4 * 1024 * 1024) return false
  try {
    const bundle = record(JSON.parse(file.textSync()))
    const envelope = record(bundle.envelope), header = record(envelope.header)
    return bundle.version === 1 && typeof bundle.network_id === 'string' && /^[a-f0-9]{64}$/.test(bundle.network_id)
      && header.format === 'vibeface-recovery' && header.version === 1 && header.kdf === 'argon2id'
      && header.memory_kib === 19_456 && header.iterations === 2 && header.parallelism === 1 && header.cipher === 'xchacha20poly1305'
      && typeof header.salt === 'string' && /^[a-f0-9]{32}$/.test(header.salt)
      && typeof header.nonce === 'string' && /^[a-f0-9]{48}$/.test(header.nonce)
      && typeof envelope.ciphertext === 'string' && envelope.ciphertext.length >= 32 && envelope.ciphertext.length % 2 === 0 && /^[a-f0-9]+$/.test(envelope.ciphertext)
  } catch { return false }
}
