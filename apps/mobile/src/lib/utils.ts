import { clsx, type ClassValue } from 'clsx'
import { extendTailwindMerge } from 'tailwind-merge'

// The theme's type scale; unregistered, `text-body` reads as a colour and drops `text-foreground`.
const twMerge = extendTailwindMerge({ extend: { theme: { text: ['title', 'heading', 'body', 'meta'] } } })

export function cn(...inputs: ClassValue[]) {
  return twMerge(clsx(inputs))
}
