import { createCn } from "cn/config"

// Without the theme's sizes, a merge reads text-meta as a color and drops it beside text-muted-foreground.
export const cn = createCn({ extend: { classGroups: { "font-size": [{ text: ["title", "heading", "body", "meta"] }] } } })
