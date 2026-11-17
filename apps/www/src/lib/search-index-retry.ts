// The SDK caches rejected index loads by URL. Keep the chosen URL across dialog remounts.
let generation = 0

export function searchIndexSource() {
  return `/search-index.json${generation ? `?retry=${generation}` : ''}`
}

/** Only an explicit retry chooses a fresh SDK cache entry. */
export function retrySearchIndex() {
  generation += 1
  return searchIndexSource()
}
