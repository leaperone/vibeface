import qrcode from 'qrcode-generator'
import { useMemo } from 'react'
import { View } from 'react-native'
import Svg, { Path } from 'react-native-svg'

/** A QR code drawn as one path, dark on white so any camera reads it in either theme. */
export function QrCode({ value, label, size = 224 }: { value: string; label: string; size?: number }) {
  const { count, path } = useMemo(() => {
    const code = qrcode(0, 'M')
    code.addData(value)
    code.make()
    const count = code.getModuleCount()
    let path = ''
    for (let row = 0; row < count; row++) for (let column = 0; column < count; column++) if (code.isDark(row, column)) path += `M${column} ${row}h1v1h-1z`
    return { count, path }
  }, [value])
  return <View accessible accessibilityRole="image" accessibilityLabel={label} className="self-center rounded-lg bg-white p-3">
    <Svg width={size} height={size} viewBox={`0 0 ${count} ${count}`}><Path d={path} fill="#000" /></Svg>
  </View>
}
