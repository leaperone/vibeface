import { useRef, useState, type ChangeEvent, type KeyboardEvent, type MouseEvent, type ReactNode } from 'react'
import { ArrowUpIcon, PaperclipIcon, SquareIcon, XIcon } from 'lucide-react'
import { Spinner } from '@/components/ui/spinner'
import { cn } from 'cn'
import { isSubmitKey } from '@vibeface/client/chat/protocol.ts'

import {
  InputGroup,
  InputGroupAddon,
  InputGroupButton,
  InputGroupTextarea,
} from '@/components/ui/input-group'

export function PromptInput({
  value,
  hint,
  disabled,
  busy,
  onChange,
  onSubmit,
  onStop,
  files = [],
  onFiles,
  above,
  children,
}: {
  value: string
  hint: ReactNode
  disabled: boolean
  busy: boolean
  onChange: (value: string) => void
  onSubmit: () => void
  onStop?: () => void
  files?: File[]
  onFiles?: (files: File[]) => void
  /** Floats over the transcript, just above the box. */
  above?: ReactNode
  children?: ReactNode
}) {
  const formRef = useRef<HTMLFormElement>(null)
  const pickerRef = useRef<HTMLInputElement>(null)
  // One line until the text is focused, so the transcript keeps the screen; it grows while writing.
  const [open, setOpen] = useState(false)
  // Buttons keep the focus on the text, so a tap acts instead of collapsing the box under the finger.
  const keepFocus = (event: MouseEvent) => event.preventDefault()
  // A written message is always sendable; while writing, Stop moves beside Send instead of leaving.
  const drafting = Boolean(value.trim() || files.length)
  const stop = drafting ? undefined : onStop
  return (
    <form
      ref={formRef}
      data-toast-anchor=""
      onBlur={(event) => { if (!event.currentTarget.contains(event.relatedTarget)) setOpen(false) }}
      onSubmit={(event) => {
        event.preventDefault()
        if (disabled || busy || !value.trim()) return
        onSubmit()
      }}
    >
      <div className="relative">
        <div data-toast-above="" className="absolute start-0 bottom-full flex pb-2 empty:hidden">{above}</div>
        <InputGroup className="rounded-2xl bg-card shadow-sm dark:bg-card has-[[data-slot=input-group-control]:focus-visible]:ring-2">
          <InputGroupTextarea
            id="message-text"
            name="text"
            required
            maxLength={100000}
            value={value}
            placeholder="发消息"
            disabled={disabled || busy}
            aria-label="给 Bot 发消息"
            aria-describedby={hint ? 'manager-hint' : undefined}
            title="Enter 发送，Shift+Enter 换行"
            rows={1}
            className={cn('px-4', open ? 'min-h-24 max-h-[min(20rem,calc(var(--app-height,100dvh)*0.45))] overflow-y-auto py-3' : 'h-13 min-h-13 overflow-hidden py-3.5 md:h-12 md:min-h-12 md:py-3')}
            onFocus={() => setOpen(true)}
            onChange={(event: ChangeEvent<HTMLTextAreaElement>) => onChange(event.target.value)}
            onKeyDown={(event: KeyboardEvent<HTMLTextAreaElement>) => {
              if (isSubmitKey(event.nativeEvent)) {
                event.preventDefault()
                formRef.current?.requestSubmit()
              }
            }}
          />
          <InputGroupAddon align={open ? 'block-end' : 'inline-end'} className={open ? 'flex-wrap ps-1 pe-1.5 pt-1 pb-1.5' : 'py-0 pe-1.5'}>
            {onFiles ? <>
              <input ref={pickerRef} type="file" multiple hidden onChange={(event) => { onFiles([...files, ...event.target.files ?? []]); event.target.value = '' }} />
              <InputGroupButton type="button" variant="ghost" size="icon-sm" className="relative size-9 rounded-full text-muted-foreground hover:text-foreground max-md:size-11" aria-label={files.length ? `添加附件，已选 ${files.length} 个` : '添加附件'} title="添加附件" disabled={disabled || busy} onMouseDown={keepFocus} onClick={() => pickerRef.current?.click()}>
                <PaperclipIcon aria-hidden="true" />
                {!open && files.length ? <span aria-hidden="true" className="absolute end-1 top-1 grid min-w-4 place-items-center rounded-full bg-primary px-1 text-[0.625rem] leading-4 text-primary-foreground tabular-nums">{files.length}</span> : null}
              </InputGroupButton>
              {open && files.map((file, index) => <span key={`${file.name}:${index}`} title={file.name} className="flex max-w-56 min-w-0 items-center gap-1 rounded-full bg-muted ps-3 text-meta">
                <FileName name={file.name} />
                <button type="button" onMouseDown={keepFocus} className="grid size-8 shrink-0 place-items-center rounded-full hover:bg-background" aria-label={`移除 ${file.name}`} disabled={busy} onClick={() => onFiles(files.filter((_, at) => at !== index))}><XIcon aria-hidden="true" className="size-3.5" /></button>
              </span>)}
            </> : null}
            <span className="ms-auto flex shrink-0 gap-1">
              {drafting && onStop ? <InputGroupButton type="button" variant="outline" size="icon-sm" aria-label="停止" title="停止" className="size-9 rounded-full max-md:size-11" onMouseDown={keepFocus} onClick={onStop}>
                <SquareIcon aria-hidden="true" className="fill-current" />
              </InputGroupButton> : null}
              <InputGroupButton
                type={stop ? 'button' : 'submit'}
                variant={stop ? 'outline' : 'default'}
                size="icon-sm"
                disabled={!stop && (disabled || busy || !value.trim())}
                aria-busy={busy || undefined}
                aria-label={stop ? '停止' : '发送'}
                title={stop ? '停止' : '发送'}
                className="size-9 rounded-full transition-[background-color,transform,opacity] active:scale-95 max-md:size-11"
                onMouseDown={keepFocus}
                onClick={stop}
              >
                {stop ? <SquareIcon aria-hidden="true" className="fill-current" /> : busy ? <Spinner aria-hidden="true" /> : <ArrowUpIcon aria-hidden="true" />}
              </InputGroupButton>
            </span>
          </InputGroupAddon>
        </InputGroup>
      </div>
      <div className="flex flex-wrap items-center gap-x-3 gap-y-1 pt-2 text-meta text-muted-foreground empty:hidden">
        {hint ? <p id="manager-hint">{hint}</p> : null}
        {children}
      </div>
    </form>
  )
}

/** The extension stays readable when a long name is cut. */
export function FileName({ name }: { name: string }) {
  const dot = name.lastIndexOf('.')
  if (dot <= 0 || name.length - dot > 8) return <span className="truncate">{name}</span>
  return <span className="flex min-w-0"><span className="truncate">{name.slice(0, dot)}</span><span className="shrink-0">{name.slice(dot)}</span></span>
}
