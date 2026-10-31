import type { TranscriptMessage } from "../chat/types.ts"
import type { BotRecord, NodeInfo, OpenWork, Question, WorkRecord } from "./types.ts"
import { providerNames, type ProviderId } from "../lib/node.ts"

/** Each question has one answer ID, so the web, Grok and every retry write the same message. */
export const answerId = (questionId: string) => `a-${questionId}`

/** The message the Node writes for an answer, exactly as this browser signs it. */
export function answerBody(questionId: string, text: string) {
  return { type: "message_posted", message_id: answerId(questionId), role: "user", text, report_results: true, answers: questionId }
}

/**
 * What a settled question shows. `standing`: the answer the network holds. `mine`: what this browser answered.
 * When answers raced and another side's won, `elsewhere` says so instead of the person's text vanishing.
 */
export function settledAnswer(standing: string | undefined, mine: string | undefined) {
  const text = standing ?? mine
  if (text === undefined) return undefined
  return { text, elsewhere: standing !== undefined && mine !== undefined && standing !== mine }
}

/** A question's first line asks; the lines after it list what the answer applies to, each split at its first "：" into a label and a value. */
export function questionParts(text: string): { ask: string; lines: { label: string | null; value: string }[] } {
  const [ask = text, ...rest] = text.split(/\r?\n/).filter((line) => line.trim())
  return { ask, lines: rest.map((line) => {
    const colon = line.indexOf("：")
    return colon > 0 ? { label: line.slice(0, colon), value: line.slice(colon + 1) } : { label: null, value: line }
  }) }
}

/** An answer that turns the request down, so it reads as refused, not agreed. */
export const declined = (answer: string) => /^(拒绝|不允许|不同意|不行|不要|否|取消)|^(deny|decline|no)\b/i.test(answer.trim())
/** How a settled answer reads: a bare refusal says only that; a refusal with words keeps them. */
export const answerLabel = (answer: string) => declined(answer) ? /^(拒绝|不允许|不同意|不行|不要|否|取消|deny|decline|no)$/i.test(answer.trim()) ? "已拒绝" : `已拒绝 · ${answer}` : `已回答 · ${answer}`

/** Nobody answered it and the device that asked did not close it. */
export const questionOpen = (question: Question) => question.answer === null && question.closed === null

/** Still waiting on the person: open, and answered neither from this device (`answered`) nor in the chat (`folded`). */
export const awaitsAnswer = (question: Question, answered: Readonly<Record<string, string>>, folded?: ReadonlyMap<string, string>) =>
  questionOpen(question) && !(question.question_id in answered) && !folded?.has(question.question_id)

/** Only an unfinished work on this conversation, device and session can wait on the request. */
export function workApproval({ work, conversationId, questions, answered }: { work: WorkRecord; conversationId: string; questions: readonly Question[]; answered: Readonly<Record<string, string>> }): Question | undefined {
  if (work.outcome || work.state === "finished" || !work.thread_id) return undefined
  return questions.find((question) => question.conversation_id === conversationId && question.about.node === work.target_node
    && question.about.session_id === work.thread_id && question.about.request_id && awaitsAnswer(question, answered))
}

/**
 * What a question card shows under its text, the same on every screen. `approval`: a held App Server request, read as 需要你同意.
 * `task`: the running task that request holds, to link to. `local`: asked by `entry`, the device this screen goes through, which the card leaves unnamed.
 */
export function questionMeta(question: Question, works: readonly OpenWork[], bots: readonly Pick<BotRecord, "bot_id" | "conversation_id">[], entry: string | undefined) {
  const approval = Boolean(question.about.request_id)
  const work = approval ? works.find((item) => item.thread_id === question.about.session_id && !item.outcome) : undefined
  const bot = work && bots.find((item) => item.conversation_id === work.conversation_id)
  return { approval, task: work && bot ? { bot_id: bot.bot_id, work_id: work.work_id, title: work.title || "任务" } : null, local: question.about.node === entry }
}

/** The task an approval held, by title, for its answered line; any work on its session counts, finished or not, the latest first. */
export function approvalTask(question: Question, works: readonly Pick<WorkRecord, "thread_id" | "title">[]): string | null {
  const work = question.about.request_id ? [...works].reverse().find((item) => item.thread_id === question.about.session_id) : undefined
  return work ? work.title || "任务" : null
}

/** What 待处理 lists: open questions, urgent ones first, then newest first. */
export function openQuestions(questions: readonly Question[]): Question[] {
  const urgent = (question: Question) => question.severity === "urgent" ? 0 : 1
  return questions.filter(questionOpen).sort((a, b) => urgent(a) - urgent(b) || b.created_at_ms - a.created_at_ms)
}

/**
 * An answer is a user message, shown inside its question instead of as a bubble.
 * `answers`: question ID to the answer text. `hidden`: messages answering a question on screen.
 */
export function foldAnswers(messages: readonly Pick<TranscriptMessage, "message_id" | "text" | "answers">[], questions: readonly Question[]) {
  const answers = new Map<string, string>(), hidden = new Set<string>()
  for (const question of questions) if (question.answer) answers.set(question.question_id, question.answer.text)
  for (const message of messages) {
    const asked = message.answers
    if (!asked || !questions.some((question) => question.question_id === asked)) continue
    hidden.add(message.message_id)
    if (!answers.has(asked)) answers.set(asked, message.text)
  }
  return { answers, hidden }
}

/** Each item goes right before the first message written after it; items newer than every message are `rest`, oldest first. */
export function placeByTime<T extends { created_at_ms: number }>(messages: readonly Pick<TranscriptMessage, "message_id" | "created_at_ms">[], items: readonly T[]) {
  const before = new Map<string, T[]>(), rest: T[] = []
  for (const item of [...items].sort((a, b) => a.created_at_ms - b.created_at_ms)) {
    const next = messages.find((message) => message.created_at_ms > item.created_at_ms)
    if (next) before.set(next.message_id, [...before.get(next.message_id) ?? [], item])
    else rest.push(item)
  }
  return { before, rest }
}

export const projectName = (cwd: string) => cwd.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || cwd

/**
 * A question's or session's folder: `name` its last segment, `path` in full. A device shares that folder as `workspace-N/…`, its
 * N-th folder counted from 1, so the full path stays off shared records; the folders it lists to every device turn it back.
 */
export function sharedProject(cwd: string, node: string, nodes: readonly Pick<NodeInfo, "device_id" | "capabilities">[]) {
  const [, index, rest] = /^workspace-(\d+)(?:[\\/](.*))?$/.exec(cwd) ?? []
  const root = index ? nodes.find((item) => item.device_id === node)?.capabilities?.roots[Number(index) - 1] : undefined
  const path = root ? rest ? `${root.replace(/[\\/]+$/, "")}/${rest}` : root : cwd
  return { name: projectName(path), path }
}

export const agentName = (agent: ProviderId) => providerNames[agent]
