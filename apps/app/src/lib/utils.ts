type ClassValue = string | number | false | null | undefined | ClassValue[] | { [className: string]: unknown }

export function cn(...values: ClassValue[]): string {
  return values.flatMap(value => {
    if (!value) return []
    if (typeof value === 'string' || typeof value === 'number') return [String(value)]
    if (Array.isArray(value)) return [cn(...value)]
    return Object.keys(value).filter(key => value[key])
  }).join(' ')
}
