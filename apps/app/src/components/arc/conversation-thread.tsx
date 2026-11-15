import { Message,type ChatMessage } from '@/features/chat/elements/message'
import { messageTimes } from '@vibeface/client/chat/timeline.ts'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { ArrowDown } from 'lucide-react'
import { useEffect,useLayoutEffect,useRef,useState,type ReactNode } from 'react'
import styles from './conversation.module.css'

export type ChatEntry =
  | { key: string; anchor?: boolean; message: ChatMessage; conversationId?: string; schedule?: string; retry?: () => Promise<void>; detail?: string }
  | { key: string; anchor?: undefined; node: ReactNode }

/** Arc chat-thread with unread navigation, anchored history and live streamed content. */
export function Chat({ entries, empty }: { entries: ChatEntry[]; empty: ReactNode }) {
  const viewport = useRef<HTMLDivElement>(null)
  const content = useRef<HTMLDivElement>(null)
  const anchor = useRef<{ key: string; offset: number } | null>(null)
  const reading = useRef(false)
  const [away,setAway] = useState(false)
  const [now,setNow] = useState(0)
  const unreadShown = useRef(false)
  const newestSend = entries.filter(entry => entry.anchor === true).at(-1)?.key
  const lastSend = useRef(newestSend)
  function latest() { const view = viewport.current; if (view) view.scrollTo({ top: view.scrollHeight, behavior: matchMedia('(prefers-reduced-motion: reduce)').matches ? 'auto' : 'smooth' }); reading.current = false; anchor.current = null; setAway(false) }
  function capture() {
    const view = viewport.current
    if (!view) return
    const next = view.scrollHeight - view.clientHeight - view.scrollTop > 96
    reading.current = next; setAway(next)
    if (!next) { anchor.current = null; return }
    const top = view.getBoundingClientRect().top
    const row = [...view.querySelectorAll<HTMLElement>('[data-history-key]')].find(item => item.dataset.historyKey !== 'history-controls' && item.getBoundingClientRect().bottom > top)
    if (row?.dataset.historyKey) anchor.current = { key: row.dataset.historyKey, offset: row.getBoundingClientRect().top - top }
  }
  function place() {
    const view = viewport.current
    if (!view) return
    if (!reading.current) { view.scrollTop = view.scrollHeight; return }
    const saved = anchor.current
    if (!saved) return
    const row = [...view.querySelectorAll<HTMLElement>('[data-history-key]')].find(item => item.dataset.historyKey === saved.key)
    if (row) view.scrollTop += row.getBoundingClientRect().top - view.getBoundingClientRect().top - saved.offset
  }
  useLayoutEffect(() => {
    if (!entries.length) return
    if (newestSend !== lastSend.current) { lastSend.current = newestSend; reading.current = false; anchor.current = null }
    place()
    if (!unreadShown.current && entries.some(entry => entry.key === 'unread')) {
      unreadShown.current = true
      viewport.current?.querySelector('[data-history-key="unread"]')?.scrollIntoView({ block: 'nearest' })
      capture()
    }
  },[entries,newestSend])
  useEffect(() => { setNow(Date.now()) },[])
  useEffect(() => { const node = content.current; if (!node) return; const resize = new ResizeObserver(place); resize.observe(node); return () => resize.disconnect() },[entries.length === 0])
  const times = messageTimes(entries.flatMap(entry => 'message' in entry && entry.message.created_at_ms ? [entry.message] : []),now)
  if (!entries.length) return <div className="flex min-h-0 flex-1 flex-col overflow-y-auto">{empty}</div>
  return <div className={styles.thread}><div ref={viewport} onScroll={capture} className={styles.viewport} role="log" aria-label="聊天记录" aria-live="polite" aria-relevant="additions"><div ref={content} className={styles.messages}>{entries.map(entry => <div key={entry.key} data-history-key={entry.key} className={styles.entry}>{'message' in entry ? <Message message={entry.message} conversationId={entry.conversationId} schedule={entry.schedule} time={times.get(entry.message.message_id)} retry={entry.retry} detail={entry.detail} /> : entry.node}</div>)}</div></div>{away ? <Button type="button" variant="secondary" className={styles.latest} aria-label="回到最新消息" title="回到最新" onClick={latest}><ArrowDown aria-hidden="true" /></Button> : null}</div>
}
