import 'react-native-url-polyfill/auto'
import { CryptoDigestAlgorithm, digest, getRandomValues, randomUUID } from 'expo-crypto'

// Hermes has no Web Crypto; the shared client needs random bytes, UUIDs and SHA-256 only.
const crypto = (globalThis.crypto ??= {} as Crypto) as { getRandomValues?: unknown; randomUUID?: unknown; subtle?: unknown }
crypto.getRandomValues ??= getRandomValues
crypto.randomUUID ??= randomUUID
crypto.subtle ??= {
  digest: (algorithm: string, data: BufferSource) => {
    if (algorithm !== 'SHA-256') throw new Error(`unsupported digest ${algorithm}`)
    return digest(CryptoDigestAlgorithm.SHA256, data)
  },
}
