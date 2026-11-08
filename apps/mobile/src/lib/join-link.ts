export { openedJoinLink as openedLink } from '@vibeface/client/identity/switch-network.ts'

let switched: string | null = null
/** The confirmed target survives the provider restart that erases the old network. */
export function keepSwitchedLink(link: string | null) { switched = link }
export function switchedLink() { return switched }
