import { AppState } from 'react-native'
import { createMMKV } from 'react-native-mmkv'
import { persistLogs, saveLogs } from '@vibeface/client/lib/web-log.ts'

/**
 * Imported by the root layout before error reporting, so a fatal error is already written to the app's store and goes
 * to the device on the next launch. The same store as the platform's, which erasing clears. Leaving the foreground
 * writes the records still waiting for the save delay, since the app may not run again until it is relaunched.
 */
const store = createMMKV({ id: 'vibeface' })
persistLogs({ getItem: (key) => store.getString(key) ?? null, setItem: (key, value) => store.set(key, value) })
AppState.addEventListener('change', state => { if (state === 'background' || state === 'inactive') saveLogs() })
