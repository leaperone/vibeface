import { Appearance } from 'react-native'
import { Uniwind } from 'uniwind'
import type { useDevice } from '@vibeface/client/identity/context.tsx'

/** Erases the device and puts the theme back to the system's, as on first launch. */
export async function factoryReset(device: ReturnType<typeof useDevice>) {
  await device.factoryReset()
  Appearance.setColorScheme('unspecified')
  Uniwind.setTheme('system')
}
