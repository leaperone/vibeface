import { TextClassContext } from '@/components/ui/text';
import { cn } from '@/lib/utils';
import { LoaderCircle, type LucideIcon, type LucideProps } from 'lucide-react-native';
import * as React from 'react';
import { useWindowDimensions } from 'react-native';
import Animated, { useReducedMotion } from 'react-native-reanimated';
import { withUniwind } from 'uniwind';

type IconProps = LucideProps & {
  as: LucideIcon;
} & React.RefAttributes<LucideIcon>;

// Without a `fill-*` class the mapping yields `fill: undefined`, which must not override Lucide's `fill="none"`.
const spin = { animationName: { to: { transform: [{ rotate: '360deg' }] } }, animationDuration: '1s', animationIterationCount: 'infinite', animationTimingFunction: 'linear' } as const;

function IconImpl({ as: IconComponent, fill, size, ...props }: IconProps) {
  const { fontScale } = useWindowDimensions();
  const reduced = useReducedMotion();
  // Symbols follow Dynamic Type at half the text's rate, so they keep pace with labels and still fit their controls.
  const scaled = typeof size === 'number' ? size * (1 + (Math.min(fontScale, 2) - 1) / 2) : size;
  const icon = <IconComponent {...props} size={scaled} {...(fill === undefined ? {} : { fill })} />;
  // The loader means work in progress, so it turns wherever it appears; with Reduce Motion it holds still.
  // Sized to the glyph, or a column stretches the wrapper and the loader orbits the row's centre.
  return IconComponent === LoaderCircle && !reduced ? <Animated.View style={[spin, { width: scaled, height: scaled }]}>{icon}</Animated.View> : icon;
}

const StyledIcon = withUniwind(IconImpl, {
  size: {
    fromClassName: 'className',
    styleProperty: 'width',
  },
  color: {
    fromClassName: 'className',
    styleProperty: 'color',
  },
  fill: {
    fromClassName: 'className',
    styleProperty: 'fill',
  },
});

/**
 * A wrapper component for Lucide icons with Uniwind `className` support via `withUniwind`.
 *
 * This component allows you to render any Lucide icon while applying utility classes
 * using `uniwind`. It avoids the need to wrap or configure each icon individually.
 *
 * @component
 * @example
 * ```tsx
 * import { ArrowRight } from 'lucide-react-native';
 * import { Icon } from '@/registry/components/ui/icon';
 *
 * <Icon as={ArrowRight} className="text-red-500 size-4" />
 * ```
 *
 * @param {LucideIcon} as - The Lucide icon component to render.
 * @param {string} className - Utility classes to style the icon using Uniwind.
 * @param {number} size - Icon size (overrides the size class).
 * @param {...LucideProps} ...props - Additional Lucide icon props passed to the "as" icon.
 */
function Icon({ as: IconComponent, className, ...props }: IconProps) {
  const textClass = React.useContext(TextClassContext);
  return (
    <StyledIcon
      as={IconComponent}
      className={cn('text-foreground size-5', textClass, className)}
      {...props}
    />
  );
}

export { Icon };
