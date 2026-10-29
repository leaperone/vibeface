import { TextClassContext } from '@/components/ui/text';
import { cn } from '@/lib/utils';
import { cva, type VariantProps } from 'class-variance-authority';
import { Platform, Pressable } from 'react-native';
import Animated, { useAnimatedStyle, useSharedValue, withTiming } from 'react-native-reanimated';
import { withUniwind } from 'uniwind';
import { cssEaseOut, quick, tap } from '@/lib/motion';

const AnimatedPressable = withUniwind(Animated.createAnimatedComponent(Pressable));
// Disabling eases; fills change at once so a theme switch never leaves a stale colour behind.
const fade = { transitionProperty: 'opacity', transitionDuration: quick, transitionTimingFunction: cssEaseOut } as const;

const buttonVariants = cva(
  cn(
    'group shrink-0 flex-row items-center justify-center gap-2 rounded-lg shadow-none',
    Platform.select({
      web: "focus-visible:border-ring focus-visible:ring-ring/50 aria-invalid:ring-destructive/20 dark:aria-invalid:ring-destructive/40 aria-invalid:border-destructive whitespace-nowrap outline-none transition-all focus-visible:ring-[3px] disabled:pointer-events-none [&_svg:not([class*='size-'])]:size-4 [&_svg]:pointer-events-none [&_svg]:shrink-0",
    })
  ),
  {
    variants: {
      variant: {
        default: cn(
          'bg-primary active:bg-primary/90 shadow-xs',
          Platform.select({ web: 'hover:bg-primary/90' })
        ),
        destructive: cn(
          'bg-destructive/10 active:bg-destructive/20 dark:bg-destructive/20 dark:active:bg-destructive/30',
          Platform.select({
            web: 'hover:bg-destructive/20 focus-visible:ring-destructive/20 dark:focus-visible:ring-destructive/40',
          })
        ),
        outline: cn(
          'border-border bg-background active:bg-accent dark:bg-input/30 dark:border-input dark:active:bg-input/50 border shadow-xs',
          Platform.select({
            web: 'hover:bg-accent dark:hover:bg-input/50',
          })
        ),
        secondary: cn(
          'bg-secondary active:bg-secondary/80',
          Platform.select({ web: 'hover:bg-secondary/80' })
        ),
        ghost: cn(
          'active:bg-accent dark:active:bg-accent/50',
          Platform.select({ web: 'hover:bg-accent dark:hover:bg-accent/50' })
        ),
        link: '',
      },
      size: {
        default: cn('min-h-11 px-4 py-2', Platform.select({ web: 'has-[>svg]:px-3' })),
        sm: cn('min-h-9 gap-1.5 px-3', Platform.select({ web: 'has-[>svg]:px-2.5' })),
        // No sm: steps; on iPad widths they would override a caller's size.
        icon: 'size-11',
      },
    },
    defaultVariants: {
      variant: 'default',
      size: 'default',
    },
  }
);

const buttonTextVariants = cva(
  cn(
    'text-foreground text-body font-sans font-medium',
    Platform.select({ web: 'pointer-events-none transition-colors' })
  ),
  {
    variants: {
      variant: {
        default: 'text-primary-foreground',
        destructive: 'text-destructive',
        outline: cn(
          'group-active:text-accent-foreground',
          Platform.select({ web: 'group-hover:text-accent-foreground' })
        ),
        secondary: 'text-secondary-foreground',
        ghost: 'group-active:text-accent-foreground',
        // Underlined at rest so a link reads apart from body text; Android draws the line in the text colour, so the press dims the text instead.
        link: cn(
          'text-primary underline decoration-muted-foreground group-active:text-primary/70',
          Platform.select({ web: 'underline-offset-4 hover:text-primary/70 group-hover:text-primary/70' })
        ),
      },
      size: {
        default: '',
        sm: '',
        icon: '',
      },
    },
    defaultVariants: {
      variant: 'default',
      size: 'default',
    },
  }
);

type ButtonProps = React.ComponentProps<typeof Pressable> &
  React.RefAttributes<typeof Pressable> &
  VariantProps<typeof buttonVariants> & {
    /** Change fill on the same frame as the content, for buttons whose icon colour swaps with the fill. */
    instant?: boolean;
  };

function Button({ className, variant, size, instant, style, onPressIn, onPressOut, ...props }: ButtonProps) {
  const scale = useSharedValue(1);
  const pressed = useAnimatedStyle(() => ({ transform: [{ scale: scale.value }] }));
  const muted = props.disabled && (variant ?? 'default') === 'default';
  return (
    <TextClassContext.Provider value={cn(buttonTextVariants({ variant, size }), muted && 'text-muted-foreground')}>
      <AnimatedPressable
        className={cn(props.disabled && !muted && 'opacity-50', buttonVariants({ variant, size }), muted && 'bg-muted shadow-none', className)}
        role="button"
        {...props}
        style={[typeof style === 'function' ? undefined : style, pressed, !instant && fade]}
        onPressIn={(event) => { scale.value = withTiming(variant === 'link' ? 1 : 0.97, tap); onPressIn?.(event); }}
        onPressOut={(event) => { scale.value = withTiming(1, tap); onPressOut?.(event); }}
      />
    </TextClassContext.Provider>
  );
}

export { Button };
export type { ButtonProps };
