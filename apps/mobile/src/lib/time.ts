/** Today shows the time, this year the date, older ones the year too; same as the web list. */
export function listTime(at: number, now: number): string {
  const date = new Date(at), today = new Date(now)
  const pad = (value: number) => String(value).padStart(2, '0')
  if (date.toDateString() === today.toDateString()) return `${pad(date.getHours())}:${pad(date.getMinutes())}`
  if (date.getFullYear() === today.getFullYear()) return `${date.getMonth() + 1}月${date.getDate()}日`
  return `${date.getFullYear()}/${date.getMonth() + 1}/${date.getDate()}`
}
