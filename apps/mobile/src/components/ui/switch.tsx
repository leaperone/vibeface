import { Switch as NativeSwitch, type SwitchProps } from 'react-native'

/** React Native pins an iOS switch to the top of its row (`alignSelf: 'flex-start'`); this one follows the row's alignment. */
export function Switch({ style, ...props }: SwitchProps) {
  return <NativeSwitch style={[{ alignSelf: 'auto' }, style]} {...props} />
}
