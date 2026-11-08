/** `vibeface://join/<ticket>` puts the ticket where a route would be; the join screen reads it from the opened URL. */
export function redirectSystemPath({ path }: { path: string; initial: boolean }) {
  return /^(vibeface:\/\/|\/)?join\/vibeface1[0-9a-z]+$/.test(path) ? '/join' : path
}
