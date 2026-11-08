import { joinTicket } from '@vibeface/client/lib/transport.ts'

/** The `vibeface://join/<ticket>` link the app was opened with, if it is one. */
export function openedLink(url: string | null): string | null {
  return url?.startsWith('vibeface://join/') && joinTicket(url) ? url : null
}
