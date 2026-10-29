/** The join link a `vibeface://join?link=` address carries; the raw URL keeps the ticket after `#`, which router params drop. */
export function openedLink(url: string | null): string | null {
  if (!url) return null
  try { return new URL(url).searchParams.get('link') } catch { return null }
}
