import { useRef } from 'react'

export function PromptInput({
  value,
  hint,
  disabled,
  busy,
  onChange,
  onSubmit,
}: {
  value: string
  hint: string
  disabled: boolean
  busy: boolean
  onChange: (value: string) => void
  onSubmit: () => void
}) {
  const formRef = useRef<HTMLFormElement>(null)
  return (
    <form
      ref={formRef}
      className="chat-composer"
      onSubmit={(event) => {
        event.preventDefault()
        if (disabled || busy) return
        onSubmit()
      }}
    >
      <label htmlFor="message-text">给调度 Agent 发消息</label>
      <textarea
        id="message-text"
        name="text"
        rows={3}
        required
        maxLength={100000}
        value={value}
        placeholder="描述目标，或询问某个任务的进展…"
        aria-describedby="manager-hint"
        onChange={(event) => onChange(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === 'Enter' && !event.shiftKey) {
            event.preventDefault()
            formRef.current?.requestSubmit()
          }
        }}
      />
      <div className="chat-composer-bar">
        <button type="submit" disabled={disabled || busy} aria-busy={busy || undefined}>
          发送消息
        </button>
      </div>
      <p id="manager-hint" className="chat-hint">
        {hint}
      </p>
    </form>
  )
}
