import { useSafeAreaFrame } from 'react-native-safe-area-context'

/**
 * Wide enough to keep the home list beside the open screen, as iPad split layouts do; phones never are.
 * Read from the root view's native frame, not the window, so on rotation the sidebar goes in the same layout pass as the
 * screen widens; the window size lands first and left the sidebar's strip blank for a couple of frames.
 */
export const useWide = () => useSafeAreaFrame().width >= 768
