import { Platform } from 'react-native'
import { useSafeAreaFrame } from 'react-native-safe-area-context'

/**
 * Wide enough to keep the home list beside the open screen, as iPad split layouts do; phones never are.
 * Read from the root view's native frame, not the window, so on rotation the sidebar goes in the same layout pass as the
 * screen widens; the window size lands first and left the sidebar's strip blank for a couple of frames.
 */
export const useWide = () => useSafeAreaFrame().width >= 768

/** The platform's minimum row height for a custom pressable: 44pt on iOS, 48dp on Android, as Arc buttons size themselves. */
export const touchRow = Platform.OS === 'android' ? 'min-h-12' : 'min-h-11'
