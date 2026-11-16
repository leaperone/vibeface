import { useContext, type RefAttributes } from 'react'
import { LoaderCircle, type LucideIcon, type LucideProps } from 'lucide-react-native'
import { useWindowDimensions } from 'react-native'
import Animated, { useReducedMotion } from 'react-native-reanimated'
import { withUniwind } from 'uniwind'
import { TextClassContext } from './text'
import { cn } from '@/lib/utils'

type IconProps = LucideProps & { as: LucideIcon } & RefAttributes<LucideIcon>
const spin = { animationName: { to: { transform: [{ rotate: '360deg' }] } }, animationDuration: '700ms', animationIterationCount: 'infinite', animationTimingFunction: 'linear' } as const
function Glyph({ as: Component, size = 16, fill, ...props }: IconProps) {
  const { fontScale } = useWindowDimensions()
  const reduced = useReducedMotion()
  const scaled = typeof size === 'number' ? size * (1 + (Math.min(fontScale, 2) - 1) / 2) : size
  const glyph = <Component {...props} size={scaled} {...(fill === undefined ? {} : { fill })} />
  return Component === LoaderCircle && !reduced ? <Animated.View style={[spin, { width: scaled, height: scaled }]}>{glyph}</Animated.View> : glyph
}
const StyledGlyph = withUniwind(Glyph, {
  size: { fromClassName: 'className', styleProperty: 'width' },
  color: { fromClassName: 'className', styleProperty: 'color' },
  fill: { fromClassName: 'className', styleProperty: 'fill' },
})
export function Icon({ className, ...props }: IconProps) {
  const inherited = useContext(TextClassContext)
  return <StyledGlyph accessible={Boolean(props.accessibilityLabel)} strokeWidth={1.75} className={cn('size-4 text-foreground', inherited, className)} {...props} />
}
