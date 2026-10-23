import { cn } from '@/lib/utils';
import { Platform, TextInput } from 'react-native';

/** Caret and selection in the ink colour; Android's highlight is a tint so the selected text stays readable. */
export const caret = Platform.OS === 'ios'
  ? { selectionColorClassName: 'accent-foreground' }
  : { cursorColorClassName: 'accent-foreground', selectionHandleColorClassName: 'accent-foreground', selectionColorClassName: 'accent-foreground/25' };

function Input({
  className,
  ...props
}: React.ComponentProps<typeof TextInput> & React.RefAttributes<TextInput>) {
  return (
    <TextInput
      className={cn(
        'dark:bg-input/30 border-input bg-background text-foreground flex min-h-10 w-full min-w-0 flex-row items-center rounded-md border px-3 py-1 font-sans text-base leading-5 shadow-sm shadow-black/5 sm:min-h-9',
        props.editable === false &&
          cn(
            'opacity-50',
            Platform.select({ web: 'disabled:pointer-events-none disabled:cursor-not-allowed' })
          ),
        Platform.select({
          web: cn(
            'placeholder:text-muted-foreground selection:bg-primary selection:text-primary-foreground outline-none transition-[color,box-shadow] md:text-sm',
            'focus-visible:border-ring focus-visible:ring-ring/50 focus-visible:ring-[3px]',
            'aria-invalid:ring-destructive/20 dark:aria-invalid:ring-destructive/40 aria-invalid:border-destructive'
          ),
        }),
        className
      )}
      placeholderTextColorClassName="accent-muted-foreground"
      {...caret}
      {...props}
    />
  );
}

export { Input };
