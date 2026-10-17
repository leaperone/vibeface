import { useRef, type ChangeEvent, type KeyboardEvent } from 'react'
import { isSubmitKey } from '../protocol'

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
      onSubmit={(event) => {
        event.preventDefault()
        if (disabled || busy) return
        onSubmit()
      }}
    >
      <InputGroup>
        <InputGroupTextarea
          id="message-text"
          name="text"
          required
          maxLength={100000}
          value={value}
          placeholder="描述目标，或询问某个任务的进展…"
          aria-label="给调度 Agent 发消息"
          aria-describedby="manager-hint"
          className="min-h-14 p-3"
          onChange={(event: ChangeEvent<HTMLTextAreaElement>) => onChange(event.target.value)}
          onKeyDown={(event: KeyboardEvent<HTMLTextAreaElement>) => {
            if (isSubmitKey(event.nativeEvent)) {
              event.preventDefault()
              formRef.current?.requestSubmit()
            }
          }}
        />
        <InputGroupAddon align="block-end" className="pt-1">
          <InputGroupButton
            type="submit"
            variant="default"
            size="icon-sm"
            disabled={disabled || busy}
            aria-busy={busy || undefined}
            aria-label="发送消息"
            className="ml-auto rounded-full"
          >
            发送消息
          </InputGroupButton>
        </InputGroupAddon>
      </InputGroup>
      <p id="manager-hint">{hint}</p>
    </form>
  )
}
