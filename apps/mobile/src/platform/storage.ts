import type { KeyValue } from '@vibeface/client/lib/platform.ts'
import { createMMKV } from 'react-native-mmkv'

export const store = createMMKV({ id: 'vibeface' })
export const storage: KeyValue = {
  getItem: key => store.getString(key) ?? null,
  setItem: (key, value) => store.set(key, value),
}
