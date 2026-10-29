import { cn } from '@/lib/utils';
import { Slot } from '@rn-primitives/slot';
import * as React from 'react';
import { Platform, Text as RNText } from 'react-native';

const base = cn(
  'text-foreground text-body font-sans',
  Platform.select({
    web: 'select-text',
  })
);

// Headings tighten slightly, as the web's h1–h3.
const heading = 'tracking-[-0.011em]';

const TextClassContext = React.createContext<string | undefined>(undefined);

function Text({
  className,
  asChild = false,
  maxFontSizeMultiplier = 2,
  ...props
}: React.ComponentProps<typeof RNText> &
  React.RefAttributes<typeof RNText> & {
    asChild?: boolean;
  }) {
  const textClass = React.useContext(TextClassContext);
  const Component = asChild ? Slot : RNText;
  return (
    <Component
      className={cn(base, props.role === 'heading' && heading, textClass, className)}
      maxFontSizeMultiplier={maxFontSizeMultiplier}
      {...props}
    />
  );
}

export { Text, TextClassContext };
