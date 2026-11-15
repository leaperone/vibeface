import { cn } from '@/lib/utils'
import type { ComponentProps } from 'react'
import styles from './product-kit.module.css'

/** Arc agent-run record grouping, with live task and artifact children. */
export function RecordPanel({ className, ...props }: ComponentProps<'section'>) {
  return <section {...props} className={cn(styles.record, className)} />
}
export function RecordBody({ className, ...props }: ComponentProps<'div'>) {
  return <div {...props} className={cn('grid min-w-0 gap-3', className)} />
}
