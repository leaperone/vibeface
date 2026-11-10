export type MarkdownRequest = { id: number; text: string }
export type MarkdownResponse = { kind: "rendered"; id: number; html: string } | { kind: "failed"; id: number }

export function parseMarkdownRequest(value: unknown): MarkdownRequest | undefined {
  if (typeof value !== "object" || value === null || !("id" in value) || !("text" in value)) return
  if (typeof value.id !== "number" || !Number.isSafeInteger(value.id) || typeof value.text !== "string") return
  return { id: value.id, text: value.text }
}

export function parseMarkdownResponse(value: unknown): MarkdownResponse | undefined {
  if (typeof value !== "object" || value === null || !("id" in value) || !("kind" in value)) return
  if (typeof value.id !== "number" || !Number.isSafeInteger(value.id)) return
  if (value.kind === "failed") return { kind: "failed", id: value.id }
  if (value.kind === "rendered" && "html" in value && typeof value.html === "string") return { kind: "rendered", id: value.id, html: value.html }
}
