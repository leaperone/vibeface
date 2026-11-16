import { cn } from '@/lib/utils'
import type { ComponentProps } from 'react'
import styles from './product-kit.module.css'

/** Arc settings-page field grouping. Controls own their label, validation and focus. */
export function FormField({ className, ...props }: ComponentProps<'div'>) {
  return <div {...props} className={cn(styles.field, className)} />
}
export function FieldNote({ className, ...props }: ComponentProps<'p'>) {
  return <p {...props} className={cn(styles.note, className)} />
}
