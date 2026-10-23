import type { TranscriptMessage } from "../chat/types.ts"
import type { Question } from "./types.ts"

/** Questions about sessions no bot started live here, and their answers are signed for it. */
export const SESSIONS_SCOPE = "sessions"

/** Each question has one answer ID, so the web, Grok and every retry write the same message. */
export const answerId = (questionId: string) => `a-${questionId}`

/** Answers are signed for the question's chat, or for sessions. */
export const answerScope = (question: Pick<Question, "conversation_id">) => question.conversation_id ?? SESSIONS_SCOPE

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

/** What 待处理 lists: questions nobody answered, urgent ones first, then newest first. */
export function openQuestions(questions: readonly Question[]): Question[] {
  const urgent = (question: Question) => question.severity === "urgent" ? 0 : 1
  return questions.filter((question) => question.answer === null).sort((a, b) => urgent(a) - urgent(b) || b.created_at_ms - a.created_at_ms)
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

/** "/Users/me/project/web/" → "web". */
export const projectName = (cwd: string) => cwd.replace(/[\\/]+$/, "").split(/[\\/]/).pop() || cwd

const agents: Record<string, string> = { codex: "Codex", claude: "Claude Code" }
export const agentName = (agent: string) => agents[agent] ?? agent
