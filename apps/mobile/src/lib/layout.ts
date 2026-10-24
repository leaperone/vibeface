import { useWindowDimensions } from 'react-native'

/** Wide enough to keep the home list beside the open screen, as iPad split layouts do; phones never are. */
export const useWide = () => useWindowDimensions().width >= 768
