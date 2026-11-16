import { cn } from '@/lib/utils'
import { createLink } from '@tanstack/react-router'
import styles from '@vibeface/uiarc/button/button.module.css'
import type { ButtonSize,ButtonVariant } from '@vibeface/uiarc/button/button.tsx'
import { forwardRef,type AnchorHTMLAttributes } from 'react'

type ActionLinkProps = AnchorHTMLAttributes<HTMLAnchorElement> & { variant?: ButtonVariant; size?: ButtonSize }

/** Navigation retains a real href and Arc's button appearance. */
export const ActionLink = forwardRef<HTMLAnchorElement, ActionLinkProps>(function ActionLink({ variant = 'primary', size = 'md', className, ...props }, ref) {
  return <a {...props} ref={ref} className={cn(styles.button, styles[variant], styles[size], className)} />
})
export const RoutedAction = createLink(ActionLink)
