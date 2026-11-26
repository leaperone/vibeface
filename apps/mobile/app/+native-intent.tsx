/** Keep join tickets out of route params; Expo Sharing opens its own confirmation page. */
export function redirectSystemPath({ path }: { path: string; initial: boolean }) {
  if (/^(vibeface:\/\/|\/)?join\/vibeface1[0-9a-z]+$/.test(path)) return '/join'
  try {
    const url = new URL(path, 'vibeface://')
    return url.protocol === 'vibeface:' && (url.hostname === 'expo-sharing' || url.pathname === '/expo-sharing') ? '/share' : path
  } catch { return path }
}
