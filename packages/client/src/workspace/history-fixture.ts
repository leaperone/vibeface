import { historySections, parseConversationHistory } from './model.ts'

export const completeHistory = () => parseConversationHistory(Object.fromEntries(historySections.map((section) => [section, { next_cursor: null, complete: true }])))
