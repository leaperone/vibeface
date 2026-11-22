import { createMMKV } from 'react-native-mmkv'
import { persistLogs } from '@vibeface/client/lib/web-log.ts'

/**
 * Imported by the root layout before error reporting, so a fatal error is already written to the app's store and goes
 * to the device on the next launch. The same store as the platform's, which erasing clears.
 */
const store = createMMKV({ id: 'vibeface' })
persistLogs({ getItem: (key) => store.getString(key) ?? null, setItem: (key, value) => store.set(key, value) })
