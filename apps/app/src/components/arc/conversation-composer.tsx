import { isSubmitKey } from '@vibeface/client/chat/protocol.ts'
import { attachmentLimit } from '@vibeface/client/workspace/model.ts'
import { Badge } from '@vibeface/uiarc/badge/badge.tsx'
import { Button } from '@vibeface/uiarc/button/button.tsx'
import { Textarea } from '@vibeface/uiarc/textarea/textarea.tsx'
import { ArrowUp,CircleAlert,Paperclip,Square,X } from 'lucide-react'
import { useId,useRef,useState,type KeyboardEvent,type MouseEvent,type ReactNode } from 'react'
import styles from './conversation.module.css'

/** Arc ai-composer bound to a saved product draft and verified attachment delivery. */
export function PromptInput({ id, value, hint, reason, disabled, onChange, onSubmit, onStop, files = [], onFiles, above, target, sendLabel = '发送', label = '给 Bot 发消息', children }: {
  id?: string; value: string; hint: string; reason: string; disabled: boolean
  onChange(value: string): void; onSubmit(): void; onStop?: () => void
  files?: File[]; onFiles?: (files: File[]) => void; above?: ReactNode; target?: ReactNode
  sendLabel?: string; label?: string; children?: ReactNode
}) {
  const form = useRef<HTMLFormElement>(null)
  const picker = useRef<HTMLInputElement>(null)
  const hintId = useId()
  const [focused,setFocused] = useState(false)
  const unfit = (file: File) => !file.size || file.size > attachmentLimit
  const invalid = files.find(unfit)
  const sendable = !disabled && Boolean(value.trim() || files.length) && !invalid
  const note = disabled && reason ? reason : hint
  const limit = `${attachmentLimit / 2 ** 20} MiB`
  const keepFocus = (event: MouseEvent) => event.preventDefault()
  return <form ref={form} className={styles.composer} data-toast-anchor="" onBlur={event => { if (!event.currentTarget.contains(event.relatedTarget)) setFocused(false) }} onSubmit={event => { event.preventDefault(); if (sendable) onSubmit() }}>
    {target ? <div className={styles.target}>{target}</div> : null}
    <div className={styles.composerFrame}>
      <div data-toast-above="" className={styles.above}>{above}</div>
      <Textarea id={id} label={label} name="text" required={!files.length} maxLength={100000} value={value} placeholder={files.length ? '给附件写句说明' : '发消息'} aria-describedby={note ? hintId : undefined} title="Enter 发送，Shift+Enter 换行" rows={focused ? 3 : 1} onFocus={() => setFocused(true)} onChange={event => onChange(event.target.value)} onKeyDown={(event: KeyboardEvent<HTMLTextAreaElement>) => { if (isSubmitKey(event.nativeEvent)) { event.preventDefault(); form.current?.requestSubmit() } }} />
      <div className={styles.tools}>
        {onFiles ? <><input ref={picker} type="file" multiple hidden onChange={event => { onFiles([...files,...event.target.files ?? []]); event.target.value = '' }} /><Button type="button" variant="ghost" aria-label={files.length ? `添加附件，已选 ${files.length} 个` : '添加附件'} title="添加附件" disabled={disabled} onMouseDown={keepFocus} onClick={() => picker.current?.click()}><Paperclip aria-hidden="true" />{files.length ? <Badge>{files.length}</Badge> : null}</Button></> : null}
        <div className={styles.send}>{onStop ? <Button type="button" variant="secondary" aria-label="停止" title="停止" onMouseDown={keepFocus} onClick={onStop}><Square aria-hidden="true" /></Button> : null}<Button type="submit" variant="primary" disabled={!sendable} aria-label={sendLabel} title={sendLabel} onMouseDown={keepFocus}><ArrowUp aria-hidden="true" /></Button></div>
        {onFiles && files.length ? <div className={styles.attachments}>{files.map((file,index) => <span key={`${file.name}:${index}`} title={file.name} className={styles.attachment} data-invalid={unfit(file) || undefined}>{unfit(file) ? <CircleAlert aria-label={file.size ? `超过 ${limit}` : '空文件'} className="size-4 shrink-0" /> : null}<FileName name={file.name} /><Button type="button" variant="ghost" aria-label={`移除 ${file.name}`} onMouseDown={keepFocus} onClick={() => onFiles(files.filter((_,at) => at !== index))}><X aria-hidden="true" className="size-4" /></Button></span>)}</div> : null}
      </div>
    </div>
    <div className={styles.hints}>{invalid ? <p role="alert" className="flex basis-full items-center gap-2 text-danger"><CircleAlert aria-hidden="true" className="size-4 shrink-0" />{invalid.size ? `附件超过 ${limit}` : '附件是空文件'}，移除后再发送</p> : null}{note ? <p id={hintId}>{note}</p> : null}{children}</div>
  </form>
}
export function FileName({ name }: { name: string }) {
  const dot = name.lastIndexOf('.')
  if (dot <= 0 || name.length - dot > 8) return <span className="truncate">{name}</span>
  return <span className="flex min-w-0"><span className="truncate">{name.slice(0,dot)}</span><span className="shrink-0">{name.slice(dot)}</span></span>
}
