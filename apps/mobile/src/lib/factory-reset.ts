import { Appearance } from 'react-native'
import { Uniwind } from 'uniwind'
import type { useDevice } from '@vibeface/client/identity/context.tsx'
import { remoteNotifications } from '@/platform/remote-notifications'

/** Erases the device and puts the theme back to the system's, as on first launch. */
export async function factoryReset(device: ReturnType<typeof useDevice>) {
  if (device.ready?.isCurrent()) await remoteNotifications.prepareErase(device.ready)
  await device.factoryReset()
  Appearance.setColorScheme('unspecified')
  Uniwind.setTheme('system')
}
