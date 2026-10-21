"use client"

import { useRef, useState, type ReactNode } from "react"
import { BookOpen, Bot, CirclePause, Repeat, ScrollText, Star, type LucideIcon } from "lucide-react"
import { Button } from "@/components/ui/button"
import { Sheet, SheetContent, SheetDescription, SheetHeader, SheetTitle } from "@/components/ui/sheet"
import { formatWhen, routineFrequency } from "./assistant"
import { useWorkspace } from "./context"
import { ActionButton } from "./present"

/** Header button for the chat route; opens a read-only view of the assistant (docs/design/assistant-config.md section 5). */
export function AssistantPanelButton() {
  const workspace = useWorkspace()
  const [open, setOpen] = useState(false)
  const trigger = useRef<HTMLButtonElement>(null)
  const bot = workspace.selectedBot
  const view = workspace.data?.assistant
  if (!bot) return null
  return <>
    <Button ref={trigger} type="button" variant="ghost" size="icon" className="shrink-0 text-muted-foreground" aria-label="助手信息" title="助手信息" disabled={!view} onClick={() => setOpen(true)}>
      <Bot aria-hidden="true" />
    </Button>
    <Sheet open={open && Boolean(view)} onOpenChange={setOpen}>
      <SheetContent className="!w-full overflow-y-auto sm:!max-w-md" onCloseAutoFocus={(event) => { event.preventDefault(); trigger.current?.focus() }}>
        <SheetHeader className="pr-14">
          <SheetTitle className="break-words">{bot.name}</SheetTitle>
          <SheetDescription>在聊天里说一声就能改。</SheetDescription>
          {workspace.primary === bot.conversation_id
            ? <p className="flex items-center gap-1.5 text-meta text-muted-foreground"><Star aria-hidden="true" className="size-3.5 fill-current" />主助手</p>
            : <ActionButton className="justify-self-start" disabled={!workspace.writable} action={() => workspace.makePrimary(bot.conversation_id)}>设为主助手</ActionButton>}
        </SheetHeader>
        {view ? <div className="grid gap-8 px-4 pb-[max(1.5rem,env(safe-area-inset-bottom))]">
          <Section icon={ScrollText} title="说明">
            {bot.instructions.trim()
              ? <p className="whitespace-pre-wrap [overflow-wrap:anywhere]">{bot.instructions}</p>
              : <Empty>还没有说明</Empty>}
          </Section>
          <Section icon={Repeat} title="例行任务" count={view.routines.length}>
            {view.routines.length ? <ul className="grid gap-4">{view.routines.map((routine) => <li key={routine.routine_id} className="grid gap-0.5">
              <p className="font-medium [overflow-wrap:anywhere]">{routine.name}</p>
              <p className="flex flex-wrap items-center gap-x-2 text-meta text-muted-foreground tabular-nums">
                <span>{routineFrequency(routine)}</span>
                {!routine.enabled ? <span className="inline-flex items-center gap-1"><CirclePause aria-hidden="true" className="size-4" />已暂停</span>
                  : routine.next_run_at_ms !== null ? <span>下次 {formatWhen(routine.next_run_at_ms)}</span> : null}
              </p>
              <p className="line-clamp-2 text-meta text-muted-foreground [overflow-wrap:anywhere]" title={routine.prompt}>{routine.prompt}</p>
            </li>)}</ul> : <Empty>还没有例行任务</Empty>}
          </Section>
          <Section icon={BookOpen} title="共用 skill" count={view.skills.length}>
            {view.skills.length ? <ul className="grid gap-3">{view.skills.map((skill) => <li key={skill.skill_id} className="grid gap-0.5">
              <p className="font-medium [overflow-wrap:anywhere]">{skill.name}</p>
              {skill.description ? <p className="text-meta text-muted-foreground [overflow-wrap:anywhere]">{skill.description}</p> : null}
            </li>)}</ul> : <Empty>还没有 skill</Empty>}
          </Section>
        </div> : null}
      </SheetContent>
    </Sheet>
  </>
}

function Section({ icon: Icon, title, count, children }: { icon: LucideIcon; title: string; count?: number; children: ReactNode }) {
  return <section className="grid gap-3">
    <h3 className="flex items-center gap-2 text-meta font-medium text-muted-foreground">
      <Icon aria-hidden="true" className="size-4 shrink-0" />{title}
      {count ? <span className="tabular-nums">{count}</span> : null}
    </h3>
    {children}
  </section>
}

function Empty({ children }: { children: ReactNode }) {
  return <p className="text-meta text-muted-foreground">{children}</p>
}
