"use client";

/**
 * AI composer block. Ported 1:1 from the shipped chunk: the composer owns text, attachments
 * (uploading → done → ready), a `/` command and `@` mention listbox, simulated dictation, the model
 * and effort menu, and the send button that turns into a stop control ringed by reply progress.
 * The block around it runs the whole reply loop: phase timers, a requestAnimationFrame typewriter,
 * stop/retry, and the top bar that appears once the thread has a turn.
 */

import {
  forwardRef,
  useCallback,
  useEffect,
  useId,
  useImperativeHandle,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  type ComponentType,
  type MouseEvent as ReactMouseEvent,
  type ReactNode,
  type RefObject,
  type SVGProps,
} from "react";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import {
  ArrowUp,
  AtSign,
  Brain,
  Bug,
  Camera,
  Check,
  ChevronDown,
  CircleStop,
  ClipboardPaste,
  Code,
  FileArchive,
  FileSpreadsheet,
  FileText,
  Globe,
  Image as ImageIcon,
  Languages,
  Lightbulb,
  Mic,
  PenLine,
  Plus,
  RotateCcw,
  SquarePen,
  TriangleAlert,
  Upload,
  Wrench,
  X,
  type LucideIcon,
} from "lucide-react";
import { CopyButton } from "../copy-button/copy-button";
import { TextShimmer } from "../text-shimmer/text-shimmer";
import { Tooltip } from "../tooltip/tooltip";
import { motionTokens } from "../lib/motion-tokens";
import styles from "./ai-composer.module.css";

const { spring, duration, ease } = motionTokens;
/** The block's shared settle: 240ms, no overshoot. */
const settle = { type: "spring", visualDuration: 0.24, bounce: 0 } as const;

const cx = (...values: Array<string | false | null | undefined>) => values.filter(Boolean).join(" ");

const EFFORTS = ["low", "medium", "high", "xhigh", "max"] as const;
type Effort = (typeof EFFORTS)[number];

const EFFORT_LABELS: Record<Effort, string> = {
  low: "Low",
  medium: "Medium",
  high: "High",
  xhigh: "Xhigh",
  max: "Max",
};

const EFFORT_HINTS: Record<Effort, string> = {
  low: "Fastest replies",
  medium: "Balanced",
  high: "More careful",
  xhigh: "Deeper reasoning",
  max: "Most thorough, slowest",
};

/** Simulated thinking time per effort, in milliseconds. */
const EFFORT_MS: Record<Effort, number> = {
  low: 900,
  medium: 1200,
  high: 1500,
  xhigh: 1800,
  max: 2200,
};

type BrandMarkName = "claude" | "openai" | "gemini";

interface ModelOption {
  id: string;
  name: string;
  short: string;
  note: string;
  mark: BrandMarkName;
  defaultEffort: Effort;
}

const MODELS: ModelOption[] = [
  {
    id: "claude-opus-5-5",
    name: "Claude Opus 5.5",
    short: "Opus 5.5",
    note: "Flagship for complex work",
    mark: "claude",
    defaultEffort: "medium",
  },
  {
    id: "claude-sonnet-5-5",
    name: "Claude Sonnet 5.5",
    short: "Sonnet 5.5",
    note: "Everyday coding and agents",
    mark: "claude",
    defaultEffort: "high",
  },
  {
    id: "claude-haiku-5-5",
    name: "Claude Haiku 5.5",
    short: "Haiku 5.5",
    note: "Fastest for quick answers",
    mark: "claude",
    defaultEffort: "medium",
  },
  {
    id: "gpt-6-1-sol",
    name: "GPT-6.1 Sol",
    short: "GPT-6.1 Sol",
    note: "OpenAI, released Sep 29",
    mark: "openai",
    defaultEffort: "medium",
  },
  {
    id: "gemini-4-argon",
    name: "Gemini 4 Argon",
    short: "Gemini 4",
    note: "Google, 2M context",
    mark: "gemini",
    defaultEffort: "medium",
  },
];

interface CommandOption {
  id: string;
  label: string;
  hint: string;
}

const COMMANDS: CommandOption[] = [
  { id: "summarize", label: "/summarize", hint: "Condense text or a file into key points" },
  { id: "translate", label: "/translate", hint: "Translate into another language" },
  { id: "explain", label: "/explain", hint: "Walk through code or an idea step by step" },
  { id: "fix", label: "/fix", hint: "Find and fix a bug in the code" },
];

type ContextKind = "code" | "sheet" | "pdf" | "notion" | "linear" | "figma";

interface ContextOption {
  id: string;
  label: string;
  group: "Files" | "Docs";
  meta: string;
  kind: ContextKind;
}

const CONTEXTS: ContextOption[] = [
  { id: "checkout", label: "checkout.tsx", group: "Files", meta: "src/app/checkout", kind: "code" },
  { id: "pricing", label: "pricing-q4.csv", group: "Files", meta: "data/finance", kind: "sheet" },
  { id: "guidelines", label: "brand-guidelines.pdf", group: "Files", meta: "Design, 4.8 MB", kind: "pdf" },
  { id: "launch", label: "Q4 launch plan", group: "Docs", meta: "Notion, edited Oct 18", kind: "notion" },
  { id: "retries", label: "Checkout retries", group: "Docs", meta: "Linear, ENG-2481", kind: "linear" },
  { id: "flow", label: "Checkout flow v3", group: "Docs", meta: "Figma, edited Oct 16", kind: "figma" },
];

type SuggestionIcon = "summary" | "draft" | "bug";

interface Suggestion {
  id: string;
  label: string;
  prompt: string;
  icon?: SuggestionIcon;
}

const SUGGESTIONS: Suggestion[] = [
  {
    id: "launch",
    label: "Summarize the Q4 launch plan",
    prompt: "/summarize @Q4 launch plan and list the open risks",
    icon: "summary",
  },
  {
    id: "reply",
    label: "Draft a reply to Priya",
    prompt: "Draft a short reply to Priya about the annual discount on the Team plan",
    icon: "draft",
  },
  {
    id: "bug",
    label: "Explain the checkout retry bug",
    prompt: "/explain why @checkout.tsx can charge a card twice on a slow network",
    icon: "bug",
  },
];

const CODE_EXTENSIONS = new Set([
  "ts",
  "tsx",
  "js",
  "jsx",
  "mjs",
  "py",
  "go",
  "rs",
  "rb",
  "java",
  "kt",
  "swift",
  "css",
  "html",
  "json",
  "sh",
  "sql",
  "yml",
  "yaml",
  "md",
]);
const SHEET_EXTENSIONS = new Set(["csv", "tsv", "xls", "xlsx", "numbers"]);
const ARCHIVE_EXTENSIONS = new Set(["zip", "tar", "gz", "rar", "7z"]);

type AttachmentKind = "pdf" | "code" | "sheet" | "doc" | "image" | "archive" | "text";

const ATTACHMENT_ICONS: Record<AttachmentKind, LucideIcon> = {
  pdf: FileText,
  code: Code,
  sheet: FileSpreadsheet,
  doc: FileText,
  image: ImageIcon,
  archive: FileArchive,
  text: FileText,
};

const SUGGESTION_ICONS: Record<SuggestionIcon, LucideIcon> = {
  summary: FileText,
  draft: PenLine,
  bug: Bug,
};

const COMMAND_ICONS: Record<string, LucideIcon> = {
  summarize: FileText,
  translate: Languages,
  explain: Lightbulb,
  fix: Wrench,
};

type AttachmentStatus = "uploading" | "done" | "ready" | "error";

interface Attachment {
  id: string;
  source: "file" | "drive" | "github" | "screenshot" | "paste";
  kind: AttachmentKind;
  name: string;
  size: number;
  meta: string;
  status: AttachmentStatus;
  progress: number;
  error?: string;
  thumb?: string;
  file?: File;
  text?: string;
}

interface SentMessage {
  text: string;
  tokens: string[];
  attachments: Attachment[];
  model: string;
  effort: Effort;
  webSearch: boolean;
  thinking: boolean;
  files?: string[];
}

interface SubmitPayload {
  text: string;
  tokens: string[];
  attachments: Attachment[];
}

type TurnPhase = "waiting" | "thinking" | "searching" | "writing" | "done" | "stopped" | "error";

interface Turn {
  id: number;
  message: SentMessage;
  model: ModelOption;
  streamed: boolean;
  phase: TurnPhase;
  source: string;
  sourceDone: boolean;
  shown: number;
  chunks: number[];
  thoughtMs?: number;
  searched?: number;
  error?: string;
}

/** Phases where the composer is still working and the send button becomes a stop control. */
const working = (phase: TurnPhase) =>
  phase === "waiting" || phase === "thinking" || phase === "searching" || phase === "writing";

/** Tokens the mirror paints as marks: `/command` or `@doc`, on a word boundary at both ends. */
interface TokenRange {
  start: number;
  end: number;
  text: string;
}

function findTokens(text: string, tokens: string[]): TokenRange[] {
  const ranges: TokenRange[] = [];
  for (const token of [...new Set(tokens)].sort((a, b) => b.length - a.length)) {
    let from = 0;
    for (;;) {
      const index = text.indexOf(token, from);
      if (index < 0) break;
      const end = index + token.length;
      const startsClean = index === 0 || /\s/.test(text[index - 1] ?? "");
      const endsClean = end === text.length || /[\s.,;:!?)]/.test(text[end] ?? "");
      if (startsClean && endsClean && !ranges.some((range) => index < range.end && end > range.start)) {
        ranges.push({ start: index, end, text: token });
      }
      from = end;
    }
  }
  return ranges.sort((a, b) => a.start - b.start);
}

interface Trigger {
  type: "/" | "@";
  start: number;
  query: string;
}

/** Which `/` or `@` the caret is inside, if any. */
function detectTrigger(text: string, caret: number, ranges: TokenRange[]): Trigger | null {
  if (ranges.some((range) => caret > range.start && caret <= range.end)) return null;
  let index = caret - 1;
  while (index >= 0 && !/\s/.test(text[index] ?? "") && text[index] !== "@" && text[index] !== "/") {
    index -= 1;
  }
  const char = text[index];
  if (index < 0 || (char !== "@" && char !== "/")) return null;
  const before = index === 0 ? "" : (text[index - 1] ?? "");
  if ((before && !/\s/.test(before)) || (char === "/" && before && before !== "\n")) return null;
  const query = text.slice(index + 1, caret);
  if (query.length > 24) return null;
  return { type: char, start: index, query };
}

function fileKind(name: string, type: string): AttachmentKind {
  if (type.startsWith("image/")) return "image";
  const extension = name.split(".").pop()?.toLowerCase() ?? "";
  if (extension === "pdf") return "pdf";
  if (CODE_EXTENSIONS.has(extension)) return "code";
  if (SHEET_EXTENSIONS.has(extension)) return "sheet";
  if (ARCHIVE_EXTENSIONS.has(extension)) return "archive";
  if (extension === "txt") return "text";
  return "doc";
}

function formatSize(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1048576) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / 1024 / 1024).toFixed(1)} MB`;
}

const SCREENSHOT_THUMB = `data:image/svg+xml,${encodeURIComponent(
  '<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 96 96"><rect width="96" height="96" fill="#f4f4f5"/><rect x="0" y="0" width="96" height="12" fill="#e4e4e7"/><circle cx="7" cy="6" r="2" fill="#d4d4d8"/><circle cx="13" cy="6" r="2" fill="#d4d4d8"/><rect x="0" y="12" width="22" height="84" fill="#fafafa"/><rect x="5" y="20" width="12" height="3" rx="1.5" fill="#d4d4d8"/><rect x="5" y="28" width="12" height="3" rx="1.5" fill="#d4d4d8"/><rect x="5" y="36" width="12" height="3" rx="1.5" fill="#d4d4d8"/><rect x="30" y="20" width="40" height="5" rx="2.5" fill="#a1a1aa"/><rect x="30" y="32" width="58" height="26" rx="4" fill="#ffffff"/><rect x="35" y="46" width="6" height="8" rx="1" fill="#71717a"/><rect x="44" y="41" width="6" height="13" rx="1" fill="#71717a"/><rect x="53" y="44" width="6" height="10" rx="1" fill="#71717a"/><rect x="62" y="37" width="6" height="17" rx="1" fill="#3f3f46"/><rect x="71" y="43" width="6" height="11" rx="1" fill="#71717a"/><rect x="30" y="64" width="27" height="22" rx="4" fill="#ffffff"/><rect x="61" y="64" width="27" height="22" rx="4" fill="#ffffff"/></svg>',
)}`;

let attachmentSeq = 0;
const nextAttachmentId = () => `att-${Date.now().toString(36)}-${(attachmentSeq += 1)}`;

interface BrandMarkProps extends SVGProps<SVGSVGElement> {
  title?: string;
}
function ClaudeMark({ title = "Claude", ...props }: BrandMarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...props}>
      <path d="m4.7144 15.9555 4.7174-2.6471.079-.2307-.079-.1275h-.2307l-.7893-.0486-2.6956-.0729-2.3375-.0971-2.2646-.1214-.5707-.1215-.5343-.7042.0546-.3522.4797-.3218.686.0608 1.5179.1032 2.2767.1578 1.6514.0972 2.4468.255h.3886l.0546-.1579-.1336-.0971-.1032-.0972L6.973 9.8356l-2.55-1.6879-1.3356-.9714-.7225-.4918-.3643-.4614-.1578-1.0078.6557-.7225.8803.0607.2246.0607.8925.686 1.9064 1.4754 2.4893 1.8336.3643.3035.1457-.1032.0182-.0728-.164-.2733-1.3539-2.4467-1.445-2.4893-.6435-1.032-.17-.6194c-.0607-.255-.1032-.4674-.1032-.7285L6.287.1335 6.6997 0l.9957.1336.419.3642.6192 1.4147 1.0018 2.2282 1.5543 3.0296.4553.8985.2429.8318.091.255h.1579v-.1457l.1275-1.706.2368-2.0947.2307-2.6957.0789-.7589.3764-.9107.7468-.4918.5828.2793.4797.686-.0668.4433-.2853 1.8517-.5586 2.9021-.3643 1.9429h.2125l.2429-.2429.9835-1.3053 1.6514-2.0643.7286-.8196.85-.9046.5464-.4311h1.0321l.759 1.1293-.34 1.1657-1.0625 1.3478-.8804 1.1414-1.2628 1.7-.7893 1.36.0729.1093.1882-.0183 2.8535-.607 1.5421-.2794 1.8396-.3157.8318.3886.091.3946-.3278.8075-1.967.4857-2.3072.4614-3.4364.8136-.0425.0304.0486.0607 1.5482.1457.6618.0364h1.621l3.0175.2247.7892.522.4736.6376-.079.4857-1.2142.6193-1.6393-.3886-3.825-.9107-1.3113-.3279h-.1822v.1093l1.0929 1.0686 2.0035 1.8092 2.5075 2.3314.1275.5768-.3218.4554-.34-.0486-2.2039-1.6575-.85-.7468-1.9246-1.621h-.1275v.17l.4432.6496 2.3436 3.5214.1214 1.0807-.17.3521-.6071.2125-.6679-.1214-1.3721-1.9246L14.38 17.959l-1.1414-1.9428-.1397.079-.674 7.2552-.3156.3703-.7286.2793-.6071-.4614-.3218-.7468.3218-1.4753.3886-1.9246.3157-1.53.2853-1.9004.17-.6314-.0121-.0425-.1397.0182-1.4328 1.9672-2.1796 2.9446-1.7243 1.8456-.4128.164-.7164-.3704.0667-.6618.4008-.5889 2.386-3.0357 1.4389-1.882.929-1.0868-.0062-.1579h-.0546l-6.3385 4.1164-1.1293.1457-.4857-.4554.0608-.7467.2307-.2429 1.9064-1.3114Z" />
    </svg>
  );
}

function OpenAIMark({ title = "OpenAI", ...props }: BrandMarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...props}>
      <path d="M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z" />
    </svg>
  );
}

function GitHubMark({ title = "GitHub", ...props }: BrandMarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...props}>
      <path d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" />
    </svg>
  );
}

function LinearMark({ title = "Linear", ...props }: BrandMarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...props}>
      <path d="M2.886 4.18A11.982 11.982 0 0 1 11.99 0C18.624 0 24 5.376 24 12.009c0 3.64-1.62 6.903-4.18 9.105L2.887 4.18ZM1.817 5.626l16.556 16.556c-.524.33-1.075.62-1.65.866L.951 7.277c.247-.575.537-1.126.866-1.65ZM.322 9.163l14.515 14.515c-.71.172-1.443.282-2.195.322L0 11.358a12 12 0 0 1 .322-2.195Zm-.17 4.862 9.823 9.824a12.02 12.02 0 0 1-9.824-9.824Z" />
    </svg>
  );
}

function NotionMark({ title = "Notion", ...props }: BrandMarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...props}>
      <path d="M4.459 4.208c.746.606 1.026.56 2.428.466l13.215-.793c.28 0 .047-.28-.046-.326L17.86 1.968c-.42-.326-.981-.7-2.055-.607L3.01 2.295c-.466.046-.56.28-.374.466zm.793 3.08v13.904c0 .747.373 1.027 1.214.98l14.523-.84c.841-.046.935-.56.935-1.167V6.354c0-.606-.233-.933-.748-.887l-15.177.887c-.56.047-.747.327-.747.933zm14.337.745c.093.42 0 .84-.42.888l-.7.14v10.264c-.608.327-1.168.514-1.635.514-.748 0-.935-.234-1.495-.933l-4.577-7.186v6.952L12.21 19s0 .84-1.168.84l-3.222.186c-.093-.186 0-.653.327-.746l.84-.233V9.854L7.822 9.76c-.094-.42.14-1.026.793-1.073l3.456-.233 4.764 7.279v-6.44l-1.215-.139c-.093-.514.28-.887.747-.933zM1.936 1.035l13.31-.98c1.634-.14 2.055-.047 3.082.7l4.249 2.986c.7.513.934.653.934 1.213v16.378c0 1.026-.373 1.634-1.68 1.726l-15.458.934c-.98.047-1.448-.093-1.962-.747l-3.129-4.06c-.56-.747-.793-1.306-.793-1.96V2.667c0-.839.374-1.54 1.447-1.632z" />
    </svg>
  );
}

function FigmaMark({ title = "Figma", ...props }: BrandMarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...props}>
      <path d="M15.852 8.981h-4.588V0h4.588c2.476 0 4.49 2.014 4.49 4.49s-2.014 4.491-4.49 4.491zM12.735 7.51h3.117c1.665 0 3.019-1.355 3.019-3.019s-1.355-3.019-3.019-3.019h-3.117V7.51zm0 1.471H8.148c-2.476 0-4.49-2.014-4.49-4.49S5.672 0 8.148 0h4.588v8.981zm-4.587-7.51c-1.665 0-3.019 1.355-3.019 3.019s1.354 3.02 3.019 3.02h3.117V1.471H8.148zm4.587 15.019H8.148c-2.476 0-4.49-2.014-4.49-4.49s2.014-4.49 4.49-4.49h4.588v8.98zM8.148 8.981c-1.665 0-3.019 1.355-3.019 3.019s1.355 3.019 3.019 3.019h3.117V8.981H8.148zM8.172 24c-2.489 0-4.515-2.014-4.515-4.49s2.014-4.49 4.49-4.49h4.588v4.441c0 2.503-2.047 4.539-4.563 4.539zm-.024-7.51a3.023 3.023 0 0 0-3.019 3.019c0 1.665 1.365 3.019 3.044 3.019 1.705 0 3.093-1.376 3.093-3.068v-2.97H8.148zm7.704 0h-.098c-2.476 0-4.49-2.014-4.49-4.49s2.014-4.49 4.49-4.49h.098c2.476 0 4.49 2.014 4.49 4.49s-2.014 4.49-4.49 4.49zm-.097-7.509c-1.665 0-3.019 1.355-3.019 3.019s1.355 3.019 3.019 3.019h.098c1.665 0 3.019-1.355 3.019-3.019s-1.355-3.019-3.019-3.019h-.098z" />
    </svg>
  );
}

function DriveMark({ title = "Google Drive", ...props }: BrandMarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...props}>
      <path d="M12.01 1.485c-2.082 0-3.754.02-3.743.047.01.02 1.708 3.001 3.774 6.62l3.76 6.574h3.76c2.081 0 3.753-.02 3.742-.047-.005-.02-1.708-3.001-3.775-6.62l-3.76-6.574zm-4.76 1.73a789.828 789.861 0 0 0-3.63 6.319L0 15.868l1.89 3.298 1.885 3.297 3.62-6.335 3.618-6.33-1.88-3.287C8.1 4.704 7.255 3.22 7.25 3.214zm2.259 12.653-.203.348c-.114.198-.96 1.672-1.88 3.287a423.93 423.948 0 0 1-1.698 2.97c-.01.026 3.24.042 7.222.042h7.244l1.796-3.157c.992-1.734 1.85-3.23 1.906-3.323l.104-.167h-7.249z" />
    </svg>
  );
}

function GeminiMark({ title = "Gemini", ...props }: BrandMarkProps) {
  const uid = useId().replace(/:/g, "");
  return (
    <svg viewBox="0 0 24 24" role="img" aria-label={title} {...props}>
      <path d="M20.616 10.835a14.147 14.147 0 01-4.45-3.001 14.111 14.111 0 01-3.678-6.452.503.503 0 00-.975 0 14.134 14.134 0 01-3.679 6.452 14.155 14.155 0 01-4.45 3.001c-.65.28-1.318.505-2.002.678a.502.502 0 000 .975c.684.172 1.35.397 2.002.677a14.147 14.147 0 014.45 3.001 14.112 14.112 0 013.679 6.453.502.502 0 00.975 0c.172-.685.397-1.351.677-2.003a14.145 14.145 0 013.001-4.45 14.113 14.113 0 016.453-3.678.503.503 0 000-.975 13.245 13.245 0 01-2.003-.678z" fill="#3186FF" />
      <path d="M20.616 10.835a14.147 14.147 0 01-4.45-3.001 14.111 14.111 0 01-3.678-6.452.503.503 0 00-.975 0 14.134 14.134 0 01-3.679 6.452 14.155 14.155 0 01-4.45 3.001c-.65.28-1.318.505-2.002.678a.502.502 0 000 .975c.684.172 1.35.397 2.002.677a14.147 14.147 0 014.45 3.001 14.112 14.112 0 013.679 6.453.502.502 0 00.975 0c.172-.685.397-1.351.677-2.003a14.145 14.145 0 013.001-4.45 14.113 14.113 0 016.453-3.678.503.503 0 000-.975 13.245 13.245 0 01-2.003-.678z" fill={`url(#g0-${uid})`} />
      <path d="M20.616 10.835a14.147 14.147 0 01-4.45-3.001 14.111 14.111 0 01-3.678-6.452.503.503 0 00-.975 0 14.134 14.134 0 01-3.679 6.452 14.155 14.155 0 01-4.45 3.001c-.65.28-1.318.505-2.002.678a.502.502 0 000 .975c.684.172 1.35.397 2.002.677a14.147 14.147 0 014.45 3.001 14.112 14.112 0 013.679 6.453.502.502 0 00.975 0c.172-.685.397-1.351.677-2.003a14.145 14.145 0 013.001-4.45 14.113 14.113 0 016.453-3.678.503.503 0 000-.975 13.245 13.245 0 01-2.003-.678z" fill={`url(#g1-${uid})`} />
      <path d="M20.616 10.835a14.147 14.147 0 01-4.45-3.001 14.111 14.111 0 01-3.678-6.452.503.503 0 00-.975 0 14.134 14.134 0 01-3.679 6.452 14.155 14.155 0 01-4.45 3.001c-.65.28-1.318.505-2.002.678a.502.502 0 000 .975c.684.172 1.35.397 2.002.677a14.147 14.147 0 014.45 3.001 14.112 14.112 0 013.679 6.453.502.502 0 00.975 0c.172-.685.397-1.351.677-2.003a14.145 14.145 0 013.001-4.45 14.113 14.113 0 016.453-3.678.503.503 0 000-.975 13.245 13.245 0 01-2.003-.678z" fill={`url(#g2-${uid})`} />
      <defs>
        <linearGradient gradientUnits="userSpaceOnUse" id={`g0-${uid}`} x1="7" x2="11" y1="15.5" y2="12">
          <stop stopColor="#08B962" />
          <stop offset="1" stopColor="#08B962" stopOpacity="0" />
        </linearGradient>
        <linearGradient gradientUnits="userSpaceOnUse" id={`g1-${uid}`} x1="8" x2="11.5" y1="5.5" y2="11">
          <stop stopColor="#F94543" />
          <stop offset="1" stopColor="#F94543" stopOpacity="0" />
        </linearGradient>
        <linearGradient gradientUnits="userSpaceOnUse" id={`g2-${uid}`} x1="3.5" x2="17.5" y1="13.5" y2="12">
          <stop stopColor="#FABC12" />
          <stop offset=".46" stopColor="#FABC12" stopOpacity="0" />
        </linearGradient>
      </defs>
    </svg>
  );
}

function ModelMark({ mark, className }: { mark: BrandMarkName; className?: string }) {
  if (mark === "claude") return <ClaudeMark className={cx(styles.modelMark, styles.claudeMark, className)} />;
  if (mark === "openai") return <OpenAIMark className={cx(styles.modelMark, className)} />;
  return <GeminiMark className={cx(styles.modelMark, className)} />;
}

/** The glyph in an attachment chip: a thumbnail when we have one, a brand mark for a source, else the file kind. */
function AttachmentGlyph({ item, size = "md" }: { item: Attachment; size?: "md" | "sm" }) {
  if (item.thumb) {
    return (
      <img
        className={size === "sm" ? styles.thumbSmall : styles.thumb}
        src={item.thumb}
        alt=""
        draggable={false}
      />
    );
  }
  if (item.source === "drive") return <DriveMark className={styles.glyph} />;
  if (item.source === "github") return <GitHubMark className={styles.glyph} />;
  if (item.source === "paste") return <ClipboardPaste className={styles.glyph} />;
  const Glyph = ATTACHMENT_ICONS[item.kind];
  return <Glyph className={styles.glyph} />;
}

/** Upload progress as a hairline ring; the chip swaps it for a tick when the file lands. */
function UploadRing({ progress }: { progress: number }) {
  return (
    <svg width={18} height={18} viewBox="0 0 18 18" aria-hidden="true" className={styles.ring}>
      <circle className={styles.ringTrack} cx="9" cy="9" r="7" />
      <circle
        className={styles.ringFill}
        cx="9"
        cy="9"
        r="7"
        pathLength={1}
        strokeDasharray="1 1"
        style={{ strokeDashoffset: 1 - progress }}
      />
    </svg>
  );
}

interface AttachmentChipProps {
  item: Attachment;
  reduced: boolean;
  onRemove: (id: string) => void;
}

function AttachmentChip({ item, reduced, onRemove }: AttachmentChipProps) {
  const uploading = item.status === "uploading";
  return (
    <motion.li
      className={styles.chip}
      data-status={item.status}
      title={item.name}
      layout={reduced ? false : "position"}
      initial={{ opacity: 0, scale: 0.92 }}
      animate={{ opacity: 1, scale: 1 }}
      exit={{ opacity: 0, scale: 0.92, transition: { duration: duration.fast, ease: [...ease.standard] } }}
    >
      <span className={cx(styles.chipGlyph, item.thumb && styles.chipGlyphImage)}>
        <AttachmentGlyph item={item} />
      </span>
      <span className={styles.chipText}>
        <span className={styles.chipName}>{item.name}</span>
        <span className={styles.chipMeta} aria-live="off">
          {item.status === "error" ? (
            <>
              <TriangleAlert className={styles.chipMetaIcon} aria-hidden="true" />
              {item.error}
            </>
          ) : uploading ? (
            <span className={styles.tabular}>Uploading {Math.round(item.progress * 100)}%</span>
          ) : (
            item.meta
          )}
        </span>
      </span>
      <button
        type="button"
        className={styles.chipAction}
        aria-label={uploading ? `Cancel upload of ${item.name}` : `Remove ${item.name}`}
        onClick={(event) => {
          event.stopPropagation();
          onRemove(item.id);
        }}
      >
        <AnimatePresence mode="popLayout" initial={false}>
          {uploading ? (
            <motion.span
              key="ring"
              className={styles.chipActionGlyph}
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
            >
              <UploadRing progress={item.progress} />
            </motion.span>
          ) : item.status === "done" ? (
            <motion.span
              key="check"
              className={styles.chipActionGlyph}
              initial={{ opacity: 0, scale: 0.6 }}
              animate={{ opacity: 1, scale: 1 }}
              exit={{ opacity: 0 }}
            >
              <Check className={styles.chipCheck} aria-hidden="true" />
            </motion.span>
          ) : (
            <motion.span
              key="remove"
              className={styles.chipActionGlyph}
              initial={{ opacity: 0, rotate: -45 }}
              animate={{ opacity: 1, rotate: 0 }}
              exit={{ opacity: 0 }}
            >
              <X aria-hidden="true" />
            </motion.span>
          )}
        </AnimatePresence>
      </button>
    </motion.li>
  );
}

interface WidthMorphProps {
  children: ReactNode;
  reduced: boolean;
  className?: string;
}

/** Springs to the width of new content instead of snapping, so the model chip never jumps. */
function WidthMorph({ children, reduced, className }: WidthMorphProps) {
  const inner = useRef<HTMLSpanElement>(null);
  const measured = useRef<string | null>(null);
  const [width, setWidth] = useState<number | null>(null);
  const [animateWidth, setAnimateWidth] = useState(false);
  useLayoutEffect(() => {
    const node = inner.current;
    if (!node) return;
    const observer = new ResizeObserver(([entry]) => {
      if (!entry) return;
      const box = entry.borderBoxSize?.[0];
      const current = node.textContent;
      setAnimateWidth(measured.current !== null && measured.current !== current);
      measured.current = current;
      setWidth(Math.ceil(box?.inlineSize ?? node.offsetWidth));
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  return (
    <motion.span
      className={cx(styles.morph, className)}
      initial={false}
      animate={width === null ? undefined : { width }}
      transition={animateWidth && !reduced ? settle : { duration: 0 }}
    >
      <span ref={inner} className={styles.morphInner}>
        {children}
      </span>
    </motion.span>
  );
}

interface SendButtonProps {
  state: "empty" | "ready" | "blocked" | "generating";
  progress: number | null;
  progressSeconds: number;
  run: number;
  label: string;
  reduced: boolean;
  onSend: () => void;
  onStop: () => void;
}

/** Send, or stop while a reply is running: the ring around it carries that reply's progress. */
function SendButton({ state, progress, progressSeconds, run, label, reduced, onSend, onStop }: SendButtonProps) {
  const generating = state === "generating";
  const enabled = state === "ready" || generating;
  const tracked = progress !== null;
  const dash = tracked ? 1 - Math.max(0.02, progress) : 0.72;
  return (
    <Tooltip content={label}>
      <motion.button
        type="button"
        className={styles.send}
        data-state={state}
        aria-label={label}
        aria-disabled={!enabled || undefined}
        whileTap={enabled && !reduced ? { scale: 0.92 } : undefined}
        transition={spring.snappy}
        onClick={() => (generating ? onStop() : onSend())}
      >
        <svg
          className={styles.sendRing}
          viewBox="0 0 44 44"
          aria-hidden="true"
          data-spin={generating && !tracked && !reduced ? "" : undefined}
        >
          <motion.circle
            className={styles.sendTrack}
            cx="22"
            cy="22"
            r="20.5"
            animate={{ opacity: generating ? 1 : 0 }}
          />
          <motion.circle
            key={run}
            className={styles.sendArc}
            cx="22"
            cy="22"
            r="20.5"
            pathLength={1}
            strokeDasharray="1 1"
            initial={{ strokeDashoffset: 1, opacity: 0 }}
            animate={{ strokeDashoffset: dash, opacity: generating ? 1 : 0 }}
            transition={{
              strokeDashoffset: { duration: Math.max(0.12, progressSeconds), ease: "linear" },
              opacity: { duration: duration.standard, delay: generating || reduced ? 0 : 0.12 },
            }}
          />
        </svg>
        <AnimatePresence mode="popLayout" initial={false}>
          {generating ? (
            <motion.span
              key="stop"
              className={styles.sendGlyph}
              initial={{ opacity: 0, scale: 0.3, borderRadius: 8 }}
              animate={{ opacity: 1, scale: 1, borderRadius: 3 }}
            >
              <span className={styles.stopGlyph} />
            </motion.span>
          ) : (
            <motion.span
              key="arrow"
              className={styles.sendGlyph}
              initial={{ opacity: 0, y: 10 }}
              animate={{ opacity: 1, y: 0 }}
              exit={{ opacity: 0, y: -14 }}
            >
              <ArrowUp aria-hidden="true" />
            </motion.span>
          )}
        </AnimatePresence>
      </motion.button>
    </Tooltip>
  );
}

interface ToolChipProps {
  tool: "search" | "thinking";
  on: boolean;
  onToggle: () => void;
}

function ToolChip({ tool, on, onToggle }: ToolChipProps) {
  const search = tool === "search";
  const label = search ? "Web search" : "Extended thinking";
  const Icon = search ? Globe : Brain;
  const hint = search
    ? on
      ? "Web search is on"
      : "Search the web while answering"
    : on
      ? "Extended thinking is on"
      : "Think longer before answering";
  return (
    <Tooltip content={hint}>
      <button type="button" className={styles.toolChip} aria-pressed={on} aria-label={label} onClick={onToggle}>
        <Icon className={styles.toolIcon} aria-hidden="true" />
        <span className={styles.toolLabel}>{label}</span>
      </button>
    </Tooltip>
  );
}

interface AddContextMenuProps {
  onUpload: () => void;
  onDrive: () => void;
  onGithub: () => void;
  onScreenshot: () => void;
  onMention: () => void;
  onClosedAfterPick: () => void;
}

function AddContextMenu({ onUpload, onDrive, onGithub, onScreenshot, onMention, onClosedAfterPick }: AddContextMenuProps) {
  const picked = useRef(false);
  const item = (key: string, Icon: ComponentType<{ className?: string }>, label: string, run: () => void) => (
    <DropdownMenu.Item
      key={key}
      className={styles.menuItem}
      onSelect={() => {
        picked.current = true;
        run();
      }}
    >
      <span className={styles.menuIcon}>
        <Icon />
      </span>
      {label}
    </DropdownMenu.Item>
  );
  return (
    <DropdownMenu.Root modal={false} onOpenChange={(open) => { if (open) picked.current = false; }}>
      <Tooltip content="Add files and context">
        <DropdownMenu.Trigger asChild>
          <button type="button" className={styles.iconButton} aria-label="Add files and context">
            <Plus aria-hidden="true" />
          </button>
        </DropdownMenu.Trigger>
      </Tooltip>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          className={styles.menu}
          side="top"
          align="start"
          sideOffset={8}
          collisionPadding={12}
          loop
          onCloseAutoFocus={(event) => {
            if (!picked.current) return;
            event.preventDefault();
            onClosedAfterPick();
          }}
        >
          {item("upload", Upload, "Upload file", onUpload)}
          {item("drive", DriveMark, "Add from Google Drive", onDrive)}
          {item("github", GitHubMark, "Add from GitHub", onGithub)}
          {item("shot", Camera, "Take a screenshot", onScreenshot)}
          <DropdownMenu.Separator className={styles.menuSeparator} />
          {item("mention", AtSign, "Mention a file or doc", onMention)}
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

interface ModelMenuProps {
  models: ModelOption[];
  model: ModelOption;
  effort: Effort;
  reduced: boolean;
  onModel: (id: string) => void;
  onEffort: (effort: Effort) => void;
}

function ModelMenu({ models, model, effort, reduced, onModel, onEffort }: ModelMenuProps) {
  const trigger = useRef<HTMLButtonElement>(null);
  const pointer = useRef(false);
  return (
    <DropdownMenu.Root modal={false}>
      <DropdownMenu.Trigger asChild>
        <button
          type="button"
          ref={trigger}
          className={styles.modelChip}
          aria-label={`Model: ${model.name}, ${EFFORT_LABELS[effort].toLowerCase()} effort`}
          onPointerDown={() => {
            pointer.current = true;
          }}
          onKeyDown={() => {
            pointer.current = false;
          }}
        >
          <ModelMark mark={model.mark} />
          <WidthMorph reduced={reduced} className={styles.modelText}>
            <span className={styles.modelName}>{model.short}</span>
            <span className={styles.modelEffort}>{EFFORT_LABELS[effort]}</span>
          </WidthMorph>
          <ChevronDown className={styles.chevron} aria-hidden="true" />
        </button>
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          className={cx(styles.menu, styles.modelMenu)}
          side="top"
          align="end"
          sideOffset={8}
          collisionPadding={12}
          loop
          onCloseAutoFocus={(event) => {
            if (!pointer.current) return;
            event.preventDefault();
            trigger.current?.focus({ preventScroll: true, focusVisible: false });
          }}
          onPointerDownCapture={() => {
            pointer.current = true;
          }}
          onKeyDownCapture={() => {
            pointer.current = false;
          }}
        >
          <DropdownMenu.RadioGroup value={model.id} onValueChange={onModel}>
            {models.map((option) => (
              <DropdownMenu.RadioItem
                key={option.id}
                value={option.id}
                className={cx(styles.menuItem, styles.modelItem)}
              >
                <ModelMark mark={option.mark} />
                <span className={styles.modelItemText}>
                  <span className={styles.modelItemName}>{option.name}</span>
                  <span className={styles.modelItemNote}>{option.note}</span>
                </span>
                <DropdownMenu.ItemIndicator className={styles.modelCheck}>
                  <Check aria-hidden="true" />
                </DropdownMenu.ItemIndicator>
              </DropdownMenu.RadioItem>
            ))}
          </DropdownMenu.RadioGroup>
          <DropdownMenu.Separator className={styles.menuSeparator} />
          <DropdownMenu.Label className={styles.effortHead}>
            <span>Effort</span>
            <span className={styles.effortHint}>{EFFORT_HINTS[effort]}</span>
          </DropdownMenu.Label>
          <DropdownMenu.RadioGroup
            value={effort}
            onValueChange={(value) => onEffort(value as Effort)}
            className={styles.effortRow}
            onKeyDown={(event) => {
              if (event.key !== "ArrowLeft" && event.key !== "ArrowRight") return;
              const items = Array.from(
                event.currentTarget.querySelectorAll<HTMLElement>("[role='menuitemradio']"),
              );
              const index = items.indexOf(document.activeElement as HTMLElement);
              if (index < 0) return;
              event.preventDefault();
              const next = items[(index + (event.key === "ArrowRight" ? 1 : -1) + items.length) % items.length];
              next?.focus();
            }}
          >
            {EFFORTS.map((value) => (
              <DropdownMenu.RadioItem
                key={value}
                value={value}
                className={styles.effortItem}
                onSelect={(event) => event.preventDefault()}
              >
                {value === effort && (
                  <motion.span
                    layoutId="ai-composer-effort"
                    className={styles.effortThumb}
                    transition={reduced ? { duration: 0 } : settle}
                  />
                )}
                <span className={styles.effortText}>{EFFORT_LABELS[value]}</span>
              </DropdownMenu.RadioItem>
            ))}
          </DropdownMenu.RadioGroup>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

/** Remaining characters, ring or warning, only shown once the message is close to the limit. */
function LengthCounter({ length, max }: { length: number; max: number }) {
  const ratio = length / max;
  const over = length > max;
  const left = max - length;
  const message = over
    ? `${(-left).toLocaleString("en-US")} characters over the limit`
    : `${left.toLocaleString("en-US")} of ${max.toLocaleString("en-US")} characters left`;
  return (
    <Tooltip content={message}>
      <span className={styles.counter} data-tone={over ? "over" : ratio >= 0.95 ? "near" : "calm"}>
        {over ? (
          <TriangleAlert className={styles.counterIcon} aria-hidden="true" />
        ) : (
          <svg className={styles.counterRing} viewBox="0 0 20 20" aria-hidden="true">
            <circle className={styles.ringTrack} cx="10" cy="10" r="8" />
            <circle
              className={styles.counterFill}
              cx="10"
              cy="10"
              r="8"
              pathLength={1}
              strokeDasharray="1 1"
              style={{ strokeDashoffset: 1 - Math.min(1, ratio) }}
            />
          </svg>
        )}
        <span className={styles.counterText}>
          {over ? `${(-left).toLocaleString("en-US")} over` : `${left.toLocaleString("en-US")} left`}
        </span>
        <span className={styles.srOnly}>{message}</span>
      </span>
    </Tooltip>
  );
}

/** Fake level meter for the simulated dictation: 56 bars fed by a shaped random walk. */
function DictationWave({ startedAt, speechMs, reduced }: { startedAt: number; speechMs: number; reduced: boolean }) {
  const bars = useRef<HTMLSpanElement>(null);
  useEffect(() => {
    const nodes = Array.from(bars.current?.children ?? []) as HTMLElement[];
    const levels = Array<number>(56).fill(0.06);
    const level = (elapsed: number, limit: number) => {
      if (elapsed > limit) return 0.05 + 0.04 * Math.random();
      const seconds = elapsed / 1000;
      return Math.min(
        1,
        0.1 +
          (0.6 * Math.abs(Math.sin(11.3 * seconds)) + 0.4 * Math.abs(Math.sin(4.7 * seconds + 1.3))) *
            (Math.sin(2.4 * seconds) > -0.55 ? 1 : 0.15) *
            (0.65 + 0.45 * Math.random()),
      );
    };
    if (reduced) {
      nodes.forEach((node, index) => {
        node.style.transform = `scaleY(${0.18 + 0.3 * Math.abs(Math.sin(0.7 * index))})`;
      });
      return;
    }
    let frame = 0;
    let last = 0;
    const step = (now: number) => {
      if (now - last > 70) {
        last = now;
        levels.shift();
        levels.push(level(Date.now() - startedAt, speechMs));
        nodes.forEach((node, index) => {
          node.style.transform = `scaleY(${Math.max(0.06, levels[index] ?? 0)})`;
        });
      }
      frame = requestAnimationFrame(step);
    };
    frame = requestAnimationFrame(step);
    return () => cancelAnimationFrame(frame);
  }, [startedAt, speechMs, reduced]);
  return (
    <span ref={bars} className={styles.wave} aria-hidden="true">
      {Array.from({ length: 56 }, (_, index) => (
        <span className={styles.bar} key={index} />
      ))}
    </span>
  );
}

function DictationTimer({ from }: { from: number }) {
  const [now, setNow] = useState(from);
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 250);
    return () => window.clearInterval(timer);
  }, []);
  const seconds = Math.max(0, Math.floor((now - from) / 1000));
  return (
    <span className={cx(styles.timer, styles.tabular)}>
      {Math.floor(seconds / 60)}:{String(seconds % 60).padStart(2, "0")}
    </span>
  );
}

interface MirrorProps {
  text: string;
  placeholder: string;
  ranges: TokenRange[];
  anchor: number | null;
  anchorRef: RefObject<HTMLSpanElement | null>;
}

/** Paints the text the textarea cannot: placeholder while empty, tinted `/` and `@` tokens, and the popover anchor. */
function Mirror({ text, placeholder, ranges, anchor, anchorRef }: MirrorProps) {
  if (!text) return <span className={styles.mirrorPlaceholder}>{placeholder}</span>;
  const nodes: ReactNode[] = [];
  let cursor = 0;
  const push = (end: number) => {
    if (end <= cursor) return;
    if (anchor !== null && anchor >= cursor && anchor < end) {
      nodes.push(text.slice(cursor, anchor));
      nodes.push(<span ref={anchorRef} key="anchor" />);
      nodes.push(text.slice(anchor, end));
    } else {
      nodes.push(text.slice(cursor, end));
    }
    cursor = end;
  };
  for (const range of ranges) {
    push(range.start);
    nodes.push(
      <mark
        key={range.start}
        className={styles.token}
        data-kind={range.text.startsWith("/") ? "command" : "mention"}
      >
        {range.text}
      </mark>,
    );
    cursor = range.end;
  }
  push(text.length);
  return (
    <>
      {nodes}
      {"\u200b"}
    </>
  );
}

const ALL_TOKENS = [...COMMANDS.map((command) => command.label), ...CONTEXTS.map((context) => `@${context.label}`)];

export interface ComposerHandle {
  focus: () => void;
  fill: (text: string) => void;
  addFiles: (files: File[]) => void;
}

interface ComposerProps {
  models: ModelOption[];
  model: ModelOption;
  effort: Effort;
  webSearch: boolean;
  thinking: boolean;
  onModel: (id: string) => void;
  onEffort: (effort: Effort) => void;
  onTool: (tool: "search" | "thinking") => void;
  busy: boolean;
  progress: number | null;
  progressSeconds: number;
  run: number;
  placeholder: string;
  maxLength: number;
  reduced: boolean;
  onSubmit: (payload: SubmitPayload) => void;
  onStop: () => void;
  announce: (message: string) => void;
}

type Option = { id: string; command: CommandOption } | { id: string; group: string; context: ContextOption };

const Composer = forwardRef<ComposerHandle, ComposerProps>(function Composer(
  {
    models,
    model,
    effort,
    webSearch,
    thinking,
    onModel,
    onEffort,
    onTool,
    busy,
    progress,
    progressSeconds,
    run,
    placeholder,
    maxLength,
    reduced,
    onSubmit,
    onStop,
    announce,
  },
  ref,
) {
  const [value, setValue] = useState("");
  const [tokens, setTokens] = useState<string[]>([]);
  const [caret, setCaret] = useState(0);
  const [focused, setFocused] = useState(false);
  const [suppressedStart, setSuppressedStart] = useState<number | null>(null);
  const [list, setList] = useState({ key: "", index: 0 });
  const [attachments, setAttachments] = useState<Attachment[]>([]);
  const [dictation, setDictation] = useState<{ startedAt: number; phase: "listening" | "transcribing" } | null>(
    null,
  );
  const [height, setHeight] = useState(40);
  const [listPos, setListPos] = useState<{ left: number; bottom: number } | null>(null);

  const surface = useRef<HTMLDivElement>(null);
  const input = useRef<HTMLTextAreaElement>(null);
  const backdrop = useRef<HTMLDivElement>(null);
  const mirror = useRef<HTMLDivElement>(null);
  const anchor = useRef<HTMLSpanElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const lastCaret = useRef(0);
  const dictationInsert = useRef<number | null>(null);
  const objectUrls = useRef(new Set<string>());
  const listId = useId();
  const hintId = useId();

  const ranges = useMemo(() => findTokens(value, tokens), [value, tokens]);
  const caretInfo = useMemo(
    () => (focused && !dictation ? detectTrigger(value, caret, ranges) : null),
    [focused, dictation, value, caret, ranges],
  );
  const trigger = caretInfo && caretInfo.start !== suppressedStart ? caretInfo : null;
  const results = useMemo<Option[]>(() => {
    if (!trigger) return [];
    const query = trigger.query.toLowerCase();
    if (trigger.type === "/") {
      return COMMANDS.filter((command) => command.id.startsWith(query)).map((command) => ({
        id: command.id,
        command,
      }));
    }
    return CONTEXTS.filter(
      (context) =>
        context.label.toLowerCase().includes(query) || context.meta.toLowerCase().includes(query),
    ).map((context) => ({ id: context.id, group: context.group, context }));
  }, [trigger]);
  const open = !!(trigger && (results.length || trigger.type === "@"));
  const listKey = trigger ? `${trigger.type}${trigger.start}${trigger.query}` : "";
  const activeIndex = Math.min(
    list.key === listKey ? list.index : 0,
    Math.max(0, results.length - 1),
  );
  const setIndex = (index: number) => setList({ key: listKey, index });
  const activeOption: Option | undefined = open ? results[activeIndex] : undefined;

  /* Textarea growth: the mirror is the real box, the textarea follows its height. */
  useLayoutEffect(() => {
    const node = mirror.current;
    if (!node) return;
    const observer = new ResizeObserver(() => {
      setHeight(Math.min(208, Math.max(40, Math.ceil(node.offsetHeight))));
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  /* The command / mention popover hangs off the trigger character, measured from the surface box. */
  useLayoutEffect(() => {
    if (!open) return;
    const anchorNode = anchor.current;
    const surfaceNode = surface.current;
    if (!anchorNode || !surfaceNode) return;
    const anchorBox = anchorNode.getBoundingClientRect();
    const surfaceBox = surfaceNode.getBoundingClientRect();
    const width = Math.min(300, surfaceBox.width - 12);
    const left = Math.max(6, Math.min(anchorBox.left - surfaceBox.left - 12, surfaceBox.width - width - 6));
    const bottom = surfaceBox.bottom - anchorBox.top + 8;
    setListPos((previous) =>
      previous && previous.left === left && previous.bottom === bottom ? previous : { left, bottom },
    );
  }, [open, trigger?.start, value, height]);

  const uploading = attachments.some((item) => item.status === "uploading");
  useEffect(() => {
    if (!uploading) return;
    const timer = window.setInterval(() => {
      setAttachments((items) =>
        items.map((item) => {
          if (item.status !== "uploading") return item;
          const total = 700 + Math.min(1800, item.size / 6000);
          const next = Math.min(1, item.progress + 60 / total);
          return next >= 1 ? { ...item, progress: 1, status: "done" } : { ...item, progress: next };
        }),
      );
    }, 60);
    return () => window.clearInterval(timer);
  }, [uploading]);

  const doneIds = attachments
    .filter((item) => item.status === "done")
    .map((item) => item.id)
    .join(",");
  useEffect(() => {
    if (!doneIds) return;
    const ids = doneIds.split(",");
    const names = attachments.filter((item) => ids.includes(item.id)).map((item) => item.name);
    announce(names.length === 1 ? `${names[0]} uploaded` : `${names.length} files uploaded`);
    const timer = window.setTimeout(() => {
      setAttachments((items) =>
        items.map((item) =>
          ids.includes(item.id) && item.status === "done" ? { ...item, status: "ready" } : item,
        ),
      );
    }, 900);
    return () => window.clearTimeout(timer);
  }, [doneIds]);

  useEffect(() => {
    const urls = objectUrls.current;
    return () => {
      urls.forEach((url) => URL.revokeObjectURL(url));
    };
  }, []);

  const focusCaret = useCallback((next?: number) => {
    const node = input.current;
    if (!node) return;
    node.focus({ preventScroll: true });
    if (next !== undefined) {
      node.setSelectionRange(next, next);
      lastCaret.current = next;
      setCaret(next);
    }
  }, []);

  const replaceRange = useCallback(
    (start: number, end: number, text: string) => {
      const node = input.current;
      if (!node) return;
      node.focus({ preventScroll: true });
      node.setSelectionRange(start, end);
      const applied =
        (text ? document.execCommand("insertText", false, text) : document.execCommand("delete")) ||
        false;
      if (!applied) {
        setValue(node.value.slice(0, start) + text + node.value.slice(end));
        requestAnimationFrame(() => focusCaret(start + text.length));
      }
      const next = start + text.length;
      lastCaret.current = next;
      setCaret(next);
    },
    [focusCaret],
  );

  const addAttachments = useCallback(
    (items: Attachment[]) => {
      if (!items.length) return;
      setAttachments((current) => [...current, ...items]);
      const errors = items.filter((item) => item.status === "error");
      if (errors.length) {
        announce(`${errors.map((item) => item.name).join(", ")} is over the 30 MB limit`);
      } else {
        announce(items.length === 1 ? `Uploading ${items[0]?.name}` : `Uploading ${items.length} files`);
      }
    },
    [announce],
  );

  const addFiles = useCallback(
    (files: File[]) => {
      addAttachments(
        files.map((file) => {
          const kind = fileKind(file.name, file.type);
          const thumb = kind === "image" ? URL.createObjectURL(file) : undefined;
          if (thumb) objectUrls.current.add(thumb);
          const tooBig = file.size > 0x1e00000;
          const extension = file.name.includes(".")
            ? file.name.split(".").pop()?.toUpperCase()
            : undefined;
          return {
            id: nextAttachmentId(),
            source: "file",
            kind,
            name: file.name || "Pasted image",
            size: file.size,
            file,
            thumb,
            meta: `${kind === "image" ? "Image" : (extension ?? "File")}, ${formatSize(file.size)}`,
            status: tooBig ? "error" : "uploading",
            progress: 0,
            error: tooBig ? "Over 30 MB" : undefined,
          } satisfies Attachment;
        }),
      );
    },
    [addAttachments],
  );

  const fill = useCallback(
    (text: string) => {
      setTokens(ALL_TOKENS.filter((token) => text.includes(token)));
      const node = input.current;
      if (!node) return;
      replaceRange(0, node.value.length, text);
      if (node.value !== text) setValue(text);
    },
    [replaceRange],
  );

  function select(option: Option) {
    if (!trigger) return;
    const token = "command" in option ? option.command.label : `@${option.context.label}`;
    setTokens((current) => [...current, token]);
    replaceRange(trigger.start, caret, `${token} `);
    announce(
      "command" in option
        ? `${token} command added`
        : `${option.context.label} added as context`,
    );
  }

  useImperativeHandle(ref, () => ({ focus: () => focusCaret(), fill, addFiles }), [focusCaret, fill, addFiles]);

  const canSend = value.trim().length > 0;
  const sendable = attachments.filter((item) => item.status !== "error");
  const overLimit = value.length > maxLength;
  const state: "empty" | "ready" | "blocked" | "generating" = busy
    ? "generating"
    : canSend || sendable.length
      ? uploading || overLimit || dictation
        ? "blocked"
        : "ready"
      : "empty";
  const sendLabel =
    state === "generating"
      ? "Stop generating"
      : state === "empty"
        ? "Send message"
        : uploading
          ? "Send, after uploads finish"
          : overLimit
            ? "Send, message is over the limit"
            : "Send message";

  function submit() {
    if (state !== "ready") return;
    const found = findTokens(value, tokens).map((range) => range.text);
    onSubmit({ text: value.trim(), tokens: [...new Set(found)], attachments: sendable });
    setValue("");
    setTokens([]);
    setAttachments([]);
    setCaret(0);
    lastCaret.current = 0;
  }

  const simulateAdd = (item: Omit<Attachment, "id" | "status" | "progress">, message: string) => {
    setAttachments((current) => [...current, { ...item, id: nextAttachmentId(), status: "uploading", progress: 0 }]);
    announce(message);
  };

  function removeAttachment(id: string) {
    const item = attachments.find((entry) => entry.id === id);
    if (item?.thumb?.startsWith("blob:")) {
      URL.revokeObjectURL(item.thumb);
      objectUrls.current.delete(item.thumb);
    }
    if (item) {
      announce(item.status === "uploading" ? `Upload of ${item.name} canceled` : `${item.name} removed`);
    }
    setAttachments((current) => current.filter((entry) => entry.id !== id));
    focusCaret();
  }

  function cancelDictation() {
    setDictation(null);
    announce("Dictation canceled");
    focusCaret();
  }

  const finishDictation = useCallback(() => {
    setDictation((current) =>
      current && current.phase === "listening" ? { ...current, phase: "transcribing" } : current,
    );
  }, []);

  /* Simulated dictation: listen for 5.6s, then type the transcript out by the clock. */
  useEffect(() => {
    if (!dictation) return;
    if (dictation.phase === "listening") {
      const timer = window.setTimeout(finishDictation, 5600);
      return () => window.clearTimeout(timer);
    }
    const progress = Math.min(1, (Date.now() - dictation.startedAt) / 4200);
    const timer = window.setTimeout(
      () => {
        const words =
          "Move the launch review to Thursday, October 29, and ask Priya to bring the updated pricing numbers.".split(
            " ",
          );
        const count = progress >= 1 ? words.length : Math.max(0, Math.round(words.length * progress));
        const text = words.slice(0, count).join(" ");
        setDictation(null);
        if (!text) {
          announce("Nothing was heard");
          focusCaret();
          return;
        }
        const node = input.current;
        const at = Math.min(dictationInsert.current ?? value.length, node?.value.length ?? 0);
        const before = node?.value.slice(0, at) ?? "";
        const gap = before && !/\s$/.test(before) ? " " : "";
        requestAnimationFrame(() => {
          replaceRange(at, at, `${gap}${text}`);
          announce(`Inserted: ${text}`);
        });
      },
      reduced ? 120 : 520,
    );
    return () => window.clearTimeout(timer);
  }, [dictation?.phase, dictation?.startedAt]);

  const counterVisible = value.length >= 0.8 * maxLength;
  const tone = overLimit ? "over" : counterVisible ? "near" : "calm";
  useEffect(() => {
    if (tone === "near") announce(`${(maxLength - value.length).toLocaleString("en-US")} characters left`);
    else if (tone === "over")
      announce(`Over the ${maxLength.toLocaleString("en-US")} character limit. Shorten the message to send it`);
  }, [tone]);

  const groups =
    trigger?.type === "@"
      ? (["Files", "Docs"] as const)
          .map((name) => ({ name, items: results.filter((option) => "group" in option && option.group === name) }))
          .filter((group) => group.items.length)
      : null;
  const optionId = (option: Option) => `${listId}-${option.id}`;

  const renderOption = (option: Option) => {
    const isActive = option === activeOption;
    const CommandIcon = "command" in option ? (COMMAND_ICONS[option.command.id] ?? FileText) : null;
    return (
      <div
        key={option.id}
        id={optionId(option)}
        role="option"
        aria-selected={isActive}
        className={styles.option}
        onMouseDown={(event) => event.preventDefault()}
        onClick={() => select(option)}
        onMouseMove={() => {
          if (!isActive) setIndex(results.indexOf(option));
        }}
      >
        {CommandIcon ? (
          <CommandIcon className={styles.optionGlyph} aria-hidden="true" />
        ) : "context" in option ? (
          <ContextGlyph kind={option.context.kind} />
        ) : null}
        <span className={styles.optionText}>
          <span className={styles.optionLabel}>
            {"command" in option ? option.command.label : option.context.label}
          </span>
          <span className={styles.optionHint}>
            {"command" in option ? option.command.hint : option.context.meta}
          </span>
        </span>
      </div>
    );
  };

  return (
    <div className={styles.composer}>
      <div
        ref={surface}
        className={styles.surface}
        data-focused={focused || undefined}
        onMouseDown={(event) => {
          const target = event.target as HTMLElement;
          if (target === event.currentTarget || target.dataset.focusArea !== undefined) {
            event.preventDefault();
            focusCaret();
          }
        }}
      >
        <AnimatePresence initial={false}>
          {attachments.length > 0 && (
            <motion.div
              key="attachments"
              className={styles.attachmentsClip}
              initial={reduced ? { opacity: 0 } : { opacity: 0, height: 0 }}
              animate={{ opacity: 1, height: "auto" }}
              exit={reduced ? { opacity: 0, transition: { duration: 0 } } : { opacity: 0, height: 0 }}
              transition={reduced ? { duration: duration.fast } : spring.smooth}
            >
              <ul className={styles.attachments} aria-label="Attachments">
                <AnimatePresence initial={false} mode="popLayout">
                  {attachments.map((item) => (
                    <AttachmentChip key={item.id} item={item} reduced={reduced} onRemove={removeAttachment} />
                  ))}
                </AnimatePresence>
              </ul>
            </motion.div>
          )}
        </AnimatePresence>

        <div className={styles.field} data-focus-area="" data-dictating={dictation ? "" : undefined}>
          <div ref={backdrop} className={styles.backdrop} aria-hidden="true">
            <div ref={mirror} className={styles.mirror}>
              <Mirror
                text={value}
                placeholder={placeholder}
                ranges={ranges}
                anchor={open ? (trigger?.start ?? null) : null}
                anchorRef={anchor}
              />
            </div>
          </div>
          <textarea
            ref={input}
            className={styles.input}
            value={value}
            rows={1}
            placeholder={placeholder}
            aria-label="Message"
            aria-describedby={hintId}
            aria-controls={open ? listId : undefined}
            aria-activedescendant={activeOption ? optionId(activeOption) : undefined}
            aria-autocomplete="list"
            readOnly={!!dictation}
            spellCheck
            style={{ height, overflowY: height >= 208 ? "auto" : "hidden" }}
            onChange={(event) => {
              const next = event.target.value;
              const position = event.target.selectionStart;
              setValue(next);
              setTokens((current) => current.filter((token) => findTokens(next, [token]).length));
              if (suppressedStart !== null && !detectTrigger(next, position, findTokens(next, tokens))) {
                setSuppressedStart(null);
              }
              lastCaret.current = position;
              setCaret(position);
            }}
            onKeyDown={(event) => {
              const node = event.currentTarget;
              if (open && trigger) {
                if (event.key === "ArrowDown" || event.key === "ArrowUp") {
                  event.preventDefault();
                  if (results.length) {
                    setIndex(
                      (activeIndex + (event.key === "ArrowDown" ? 1 : -1) + results.length) % results.length,
                    );
                  }
                  return;
                }
                if ((event.key === "Enter" || event.key === "Tab") && !event.shiftKey && activeOption) {
                  event.preventDefault();
                  select(activeOption);
                  return;
                }
                if (event.key === "Escape") {
                  event.preventDefault();
                  event.stopPropagation();
                  setSuppressedStart(trigger.start);
                  return;
                }
              }
              if (event.key === "Escape") {
                if (dictation) {
                  event.preventDefault();
                  cancelDictation();
                  return;
                }
                if (busy) {
                  event.preventDefault();
                  onStop();
                  return;
                }
              }
              if (event.key === "Enter" && !event.shiftKey && !event.nativeEvent.isComposing) {
                event.preventDefault();
                submit();
                return;
              }
              const position = node.selectionStart;
              if (position === node.selectionEnd) {
                if (event.key === "Backspace") {
                  const range = ranges.find((entry) => entry.end === position);
                  if (range) {
                    event.preventDefault();
                    replaceRange(range.start, range.end, "");
                  }
                } else if (event.key === "Delete") {
                  const range = ranges.find((entry) => entry.start === position);
                  if (range) {
                    event.preventDefault();
                    replaceRange(range.start, range.end, "");
                  }
                }
              }
            }}
            onPaste={(event) => {
              const files = Array.from(event.clipboardData.files);
              if (files.length) {
                event.preventDefault();
                addFiles(files);
                return;
              }
              const text = event.clipboardData.getData("text/plain");
              const lines = text.split(/\r\n|\r|\n/).length;
              if (lines < 12 && text.length < 1200) return;
              event.preventDefault();
              setAttachments((current) => [
                ...current,
                {
                  id: nextAttachmentId(),
                  source: "paste",
                  kind: "text",
                  name: "Pasted text",
                  meta: `${lines} ${lines === 1 ? "line" : "lines"}`,
                  size: text.length,
                  status: "ready",
                  progress: 1,
                  text,
                },
              ]);
              announce(`Long paste added as an attachment, ${lines} lines`);
            }}
            onSelect={(event) => {
              const node = event.currentTarget;
              const position = node.selectionStart;
              if (position === node.selectionEnd) {
                const range = ranges.find((entry) => position > entry.start && position < entry.end);
                if (range) {
                  const snapped =
                    Math.abs(position - lastCaret.current) <= 1
                      ? position > lastCaret.current
                        ? range.end
                        : range.start
                      : position - range.start < range.end - position
                        ? range.start
                        : range.end;
                  node.setSelectionRange(snapped, snapped);
                  lastCaret.current = snapped;
                  setCaret(snapped);
                  return;
                }
              }
              lastCaret.current = position;
              setCaret(position);
            }}
            onScroll={(event) => {
              if (backdrop.current) backdrop.current.scrollTop = event.currentTarget.scrollTop;
            }}
            onFocus={() => setFocused(true)}
            onBlur={() => setFocused(false)}
          />
          <span id={hintId} className={styles.srOnly}>
            Enter sends, Shift Enter adds a line. Type slash for commands and at for files and docs.
          </span>
          <AnimatePresence>
            {dictation && (
              <motion.div
                key="dictation"
                className={styles.dictation}
                initial={reduced ? { opacity: 0 } : { opacity: 0, y: 4 }}
                animate={{ opacity: 1, y: 0 }}
                exit={{ opacity: 0, transition: { duration: duration.fast } }}
                transition={{ duration: duration.standard, ease: [...ease.enter] }}
              >
                <Tooltip content="Cancel dictation">
                  <button
                    type="button"
                    className={styles.dictationButton}
                    aria-label="Cancel dictation"
                    onClick={cancelDictation}
                  >
                    <X aria-hidden="true" />
                  </button>
                </Tooltip>
                {dictation.phase === "listening" ? (
                  <DictationWave startedAt={dictation.startedAt} speechMs={4200} reduced={reduced} />
                ) : (
                  <span className={styles.transcribing}>
                    <TextShimmer>Transcribing</TextShimmer>
                  </span>
                )}
                <DictationTimer from={dictation.startedAt} />
                <Tooltip content="Insert the transcript">
                  <button
                    type="button"
                    className={cx(styles.dictationButton, styles.dictationDone)}
                    aria-label="Insert the transcript"
                    onClick={finishDictation}
                    disabled={dictation.phase !== "listening"}
                  >
                    <Check aria-hidden="true" />
                  </button>
                </Tooltip>
              </motion.div>
            )}
          </AnimatePresence>
        </div>

        <div className={styles.toolbar}>
          <div className={styles.toolGroup}>
            <AddContextMenu
              onUpload={() => fileInput.current?.click()}
              onDrive={() =>
                simulateAdd(
                  {
                    source: "drive",
                    kind: "sheet",
                    name: "Q4 pricing model",
                    meta: "Google Sheets, 412 KB",
                    size: 412e3,
                  },
                  "Adding Q4 pricing model from Google Drive, simulated",
                )
              }
              onGithub={() =>
                simulateAdd(
                  {
                    source: "github",
                    kind: "code",
                    name: "acme/storefront",
                    meta: "main, 214 files",
                    size: 24e5,
                  },
                  "Adding acme/storefront from GitHub, simulated",
                )
              }
              onScreenshot={() =>
                simulateAdd(
                  {
                    source: "screenshot",
                    kind: "image",
                    name: "Screenshot 2026-10-20 at 14.32.png",
                    meta: "Image, 284 KB",
                    size: 284e3,
                    thumb: SCREENSHOT_THUMB,
                  },
                  "Screenshot captured, simulated",
                )
              }
              onMention={() => {
                const node = input.current;
                if (!node) return;
                const start = node.selectionStart ?? node.value.length;
                const spaced = start > 0 && !/\s/.test(node.value[start - 1] ?? "");
                requestAnimationFrame(() => replaceRange(start, node.selectionEnd ?? start, spaced ? " @" : "@"));
              }}
              onClosedAfterPick={() => focusCaret()}
            />
            <ToolChip tool="search" on={webSearch} onToggle={() => onTool("search")} />
            <ToolChip tool="thinking" on={thinking} onToggle={() => onTool("thinking")} />
          </div>
          <div className={styles.toolGroup}>
            <AnimatePresence initial={false}>
              {counterVisible && (
                <motion.span
                  key="counter"
                  className={styles.counterWrap}
                  initial={reduced ? { opacity: 0 } : { opacity: 0, width: 0 }}
                  animate={{ opacity: 1, width: "auto" }}
                  exit={reduced ? { opacity: 0 } : { opacity: 0, width: 0 }}
                  transition={reduced ? { duration: duration.fast } : settle}
                >
                  <LengthCounter length={value.length} max={maxLength} />
                </motion.span>
              )}
            </AnimatePresence>
            <ModelMenu
              models={models}
              model={model}
              effort={effort}
              reduced={reduced}
              onModel={onModel}
              onEffort={onEffort}
            />
            <Tooltip content={dictation ? "Insert the transcript" : "Dictate"}>
              <button
                type="button"
                className={styles.iconButton}
                aria-label={dictation ? "Stop dictation and insert" : "Dictate"}
                aria-pressed={!!dictation}
                data-live={dictation ? "" : undefined}
                onClick={() => {
                  if (dictation) {
                    finishDictation();
                    return;
                  }
                  dictationInsert.current = input.current?.selectionStart ?? value.length;
                  setDictation({ startedAt: Date.now(), phase: "listening" });
                  announce("Listening. Dictation is simulated in this preview");
                }}
                disabled={busy && !dictation}
              >
                <Mic aria-hidden="true" />
              </button>
            </Tooltip>
            <SendButton
              state={state}
              progress={progress}
              progressSeconds={progressSeconds}
              run={run}
              label={sendLabel}
              reduced={reduced}
              onSend={submit}
              onStop={onStop}
            />
          </div>
        </div>

        <AnimatePresence>
          {open && listPos && trigger && (
            <motion.div
              key={trigger.type}
              id={listId}
              role="listbox"
              aria-label={trigger.type === "/" ? "Commands" : "Files and docs"}
              className={styles.pop}
              style={{ left: listPos.left, bottom: listPos.bottom }}
              initial={reduced ? { opacity: 0 } : { opacity: 0, y: 6, scale: 0.98 }}
              animate={{ opacity: 1, y: 0, scale: 1 }}
              exit={
                reduced
                  ? { opacity: 0, transition: { duration: 0 } }
                  : { opacity: 0, y: 4, scale: 0.99, transition: { duration: duration.instant } }
              }
              transition={reduced ? { duration: duration.instant } : settle}
            >
              {groups
                ? groups.length
                  ? groups.map((group) => (
                      <div key={group.name} role="group" aria-label={group.name}>
                        <div className={styles.popGroup} aria-hidden="true">
                          {group.name}
                        </div>
                        {group.items.map(renderOption)}
                      </div>
                    ))
                  : (
                      <div className={styles.popEmpty}>
                        No files or docs match &ldquo;{trigger.query}&rdquo;
                      </div>
                    )
                : results.map(renderOption)}
            </motion.div>
          )}
        </AnimatePresence>
      </div>
      <input
        ref={fileInput}
        type="file"
        multiple
        hidden
        tabIndex={-1}
        onChange={(event) => {
          addFiles(Array.from(event.target.files ?? []));
          event.target.value = "";
          focusCaret();
        }}
      />
    </div>
  );
});

/** The folder / brand glyph in a mention option. */
function ContextGlyph({ kind }: { kind: ContextKind }) {
  if (kind === "notion") return <NotionMark className={styles.optionGlyph} />;
  if (kind === "linear") return <LinearMark className={styles.optionGlyph} />;
  if (kind === "figma") return <FigmaMark className={styles.optionGlyph} />;
  const Glyph = kind === "code" ? Code : kind === "sheet" ? FileSpreadsheet : FileText;
  return <Glyph className={styles.optionGlyph} aria-hidden="true" />;
}

export type AiComposerSend = (message: SentMessage, options: { signal: AbortSignal }) => void | AsyncIterable<string>;

export interface AiComposerProps {
  onSend?: AiComposerSend;
  greeting?: string;
  suggestions?: Suggestion[];
  models?: ModelOption[];
  defaultModel?: string;
  placeholder?: string;
  maxLength?: number;
  className?: string;
}

/** Splits a sent message into plain runs and tinted tokens. */
function TokenText({ text, tokens }: { text: string; tokens: string[] }) {
  const nodes: ReactNode[] = [];
  let cursor = 0;
  for (const range of findTokens(text, tokens)) {
    if (range.start > cursor) nodes.push(text.slice(cursor, range.start));
    nodes.push(
      <span key={range.start} className={styles.sentToken}>
        {range.text}
      </span>,
    );
    cursor = range.end;
  }
  if (cursor < text.length) nodes.push(text.slice(cursor));
  return <>{nodes}</>;
}

function SentMessage({ turn, reduced }: { turn: Turn; reduced: boolean }) {
  const { message } = turn;
  return (
    <motion.div
      className={styles.sent}
      initial={reduced ? { opacity: 0 } : { opacity: 0, y: 28 }}
      animate={{ opacity: 1, y: 0 }}
      transition={
        reduced ? { duration: duration.instant } : { ...settle, opacity: { duration: duration.fast, ease: [...ease.standard] } }
      }
    >
      {message.attachments.length > 0 && (
        <ul className={styles.sentFiles} aria-label="Attachments">
          {message.attachments.map((item) => (
            <li key={item.id} className={styles.sentFile} title={item.name}>
              <span className={styles.sentGlyph}>
                <AttachmentGlyph item={item} size="sm" />
              </span>
              <span className={styles.sentName}>{item.name}</span>
            </li>
          ))}
        </ul>
      )}
      {message.text && (
        <p className={styles.bubble}>
          <TokenText text={message.text} tokens={message.tokens} />
        </p>
      )}
    </motion.div>
  );
}

/** Animates its own height to the content it holds, so a growing reply never jumps. */
function Grow({ children, reduced, reserve }: { children: ReactNode; reduced: boolean; reserve?: boolean }) {
  const inner = useRef<HTMLDivElement>(null);
  const [height, setHeight] = useState<number | string>("auto");
  useLayoutEffect(() => {
    const node = inner.current;
    if (!node) return;
    const observer = new ResizeObserver(() => setHeight(node.offsetHeight));
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  return (
    <motion.div
      className={styles.grow}
      initial={false}
      animate={{ height }}
      transition={reduced ? { duration: 0 } : { duration: duration.fast, ease: "easeOut" }}
    >
      <div ref={inner} className={styles.growInner} data-reserve={reserve || undefined}>
        {children}
      </div>
    </motion.div>
  );
}

/** Streaming text: plain runs plus the last few freshly-streamed words, which fade in. */
function ReplyText({ turn, reduced }: { turn: Turn; reduced: boolean }) {
  const text = turn.source.slice(0, turn.shown);
  const from = Math.max(0, turn.chunks.length - 6);
  const first = turn.chunks.length ? (turn.chunks[from] ?? text.length) : text.length;
  return (
    <p className={styles.replyText} aria-busy={turn.phase === "writing" || undefined}>
      {text.slice(0, first)}
      {turn.chunks.slice(from).map((start, index) => {
        const end = turn.chunks[from + index + 1] ?? text.length;
        return (
          <span key={start} className={reduced ? undefined : styles.chunk}>
            {text.slice(start, end)}
          </span>
        );
      })}
    </p>
  );
}

interface ReplyProps {
  turn: Turn;
  last: boolean;
  reduced: boolean;
  onRetry: () => void;
}

function Reply({ turn, last, reduced, onRetry }: ReplyProps) {
  const { phase, message } = turn;
  const steps: Array<{ id: string; icon: LucideIcon; active: boolean; label: string }> = [];
  if (message.thinking && !turn.streamed) {
    steps.push({
      id: "think",
      icon: Brain,
      active: phase === "thinking",
      label:
        phase === "thinking"
          ? "Thinking"
          : turn.thoughtMs
            ? `Thought for ${Math.max(1, Math.round(turn.thoughtMs / 1000))}s`
            : "Thinking stopped",
    });
  }
  if (message.webSearch && !turn.streamed) {
    steps.push({
      id: "search",
      icon: Globe,
      active: phase === "searching",
      label:
        phase === "searching" || phase === "thinking"
          ? "Searching the web"
          : turn.searched
            ? `Searched ${turn.searched} sites`
            : "Search stopped",
    });
  }
  const copyValue = turn.source.slice(0, turn.shown).trim();
  const streaming = working(phase);
  return (
    <motion.div
      className={styles.reply}
      initial={{ opacity: 0 }}
      animate={{ opacity: 1 }}
      transition={{ duration: reduced ? duration.instant : duration.fast, ease: "easeOut" }}
    >
      <div className={styles.replyHead}>
        <ModelMark mark={turn.model.mark} />
        <span>{turn.model.name}</span>
      </div>
      {steps.length > 0 && (
        <div className={styles.steps}>
          {steps.map((step) => (
            <span
              key={step.id}
              className={styles.step}
              data-active={step.active || undefined}
              data-pending={(step.id === "search" && phase === "thinking") || undefined}
            >
              <step.icon className={styles.stepIcon} aria-hidden="true" />
              <TextShimmer active={step.active}>{step.label}</TextShimmer>
            </span>
          ))}
        </div>
      )}
      <Grow reduced={reduced} reserve={streaming}>
        {phase === "waiting" ? (
          <span className={styles.pending}>
            <TextShimmer>Thinking</TextShimmer>
          </span>
        ) : turn.shown > 0 ? (
          <ReplyText turn={turn} reduced={reduced} />
        ) : null}
      </Grow>
      <AnimatePresence initial={false}>
        {!streaming && (
          <motion.div
            key="foot"
            className={styles.replyFoot}
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            transition={{ duration: reduced ? duration.instant : duration.fast, ease: "easeOut" }}
          >
            {phase === "stopped" && (
              <span className={styles.footStatus}>
                <CircleStop className={styles.footIcon} aria-hidden="true" />
                Stopped
              </span>
            )}
            {phase === "error" && (
              <span className={styles.footStatus} data-tone="error">
                <TriangleAlert className={styles.footIcon} aria-hidden="true" />
                {turn.error ?? "The reply did not finish"}
              </span>
            )}
            {copyValue && <CopyButton value={copyValue} iconOnly variant="plain" label="Copy reply" />}
            {last && (
              <Tooltip content="Run this message again">
                <button type="button" className={styles.footButton} onClick={onRetry}>
                  <RotateCcw aria-hidden="true" />
                  Retry
                </button>
              </Tooltip>
            )}
          </motion.div>
        )}
      </AnimatePresence>
    </motion.div>
  );
}

export function AiComposer({
  onSend,
  greeting = "What should we work on?",
  suggestions = SUGGESTIONS,
  models = MODELS,
  defaultModel,
  placeholder = "Ask anything, / for commands, @ to add context",
  maxLength = 4000,
  className,
}: AiComposerProps) {
  const reduced = useReducedMotion() ?? false;
  const [modelId, setModelId] = useState(defaultModel ?? models[0]?.id ?? "");
  const model = models.find((option) => option.id === modelId) ?? models[0] ?? MODELS[0];
  const [effort, setEffort] = useState<Effort>(model.defaultEffort);
  const [webSearch, setWebSearch] = useState(false);
  const [thinking, setThinking] = useState(false);
  const [turns, setTurns] = useState<Turn[]>([]);
  const [announcement, setAnnouncement] = useState("");
  const [dragging, setDragging] = useState(false);

  const composer = useRef<ComposerHandle>(null);
  const thread = useRef<HTMLDivElement>(null);
  const atBottom = useRef(true);
  const abort = useRef<AbortController | null>(null);
  const dragDepth = useRef(0);
  const nextTurnId = useRef(1);

  /* Re-announce identical strings with a trailing nbsp so screen readers actually repeat them. */
  const announce = useCallback((message: string) => {
    setAnnouncement((current) => (current === message ? `${message}\u00a0` : message));
  }, []);
  const updateTurn = useCallback((id: number, update: (turn: Turn) => Turn) => {
    setTurns((current) => current.map((turn) => (turn.id === id ? update(turn) : turn)));
  }, []);

  const lastTurn = turns[turns.length - 1];
  const busy = !!(lastTurn && working(lastTurn.phase));
  const mode = turns.length ? "thread" : "new";
  const progressInfo = (() => {
    if (!lastTurn) return { progress: 0, seconds: 0 };
    if (lastTurn.streamed) return { progress: working(lastTurn.phase) ? null : 1, seconds: 0.12 };
    const base = lastTurn.message.thinking || lastTurn.message.webSearch ? 0.25 : 0.1;
    switch (lastTurn.phase) {
      case "waiting":
        return { progress: base, seconds: 0.65 };
      case "thinking":
        return { progress: lastTurn.message.webSearch ? 0.14 : base, seconds: EFFORT_MS[lastTurn.message.effort] / 1000 };
      case "searching":
        return { progress: base, seconds: 1.1 };
      case "writing":
        return {
          progress: base + (1 - base) * (lastTurn.source.length ? lastTurn.shown / lastTurn.source.length : 0),
          seconds: 0.034,
        };
      default:
        return { progress: 1, seconds: 0.1 };
    }
  })();
  const phaseKey =
    lastTurn && !lastTurn.streamed && working(lastTurn.phase) && lastTurn.phase !== "writing"
      ? `${lastTurn.id}:${lastTurn.phase}`
      : "";

  /* Phase clock: thinking → searching → writing, each phase timed by effort and tools. */
  useEffect(() => {
    if (!lastTurn || !phaseKey) return;
    const { id, phase, message } = lastTurn;
    const delay = phase === "thinking" ? EFFORT_MS[message.effort] : phase === "searching" ? 1100 : 650;
    const timer = window.setTimeout(
      () =>
        updateTurn(id, (turn) =>
          turn.phase !== phase
            ? turn
            : phase === "thinking"
              ? { ...turn, phase: message.webSearch ? "searching" : "writing", thoughtMs: delay }
              : phase === "searching"
                ? { ...turn, phase: "writing", searched: 6 }
                : { ...turn, phase: "writing" },
        ),
      delay,
    );
    return () => window.clearTimeout(timer);
  }, [phaseKey]);

  const writingId = lastTurn?.phase === "writing" ? lastTurn.id : 0;
  /* Typewriter: ~480 chars/s, snapped forward to the next word boundary. */
  useEffect(() => {
    if (!writingId) return;
    let frame = 0;
    let last = 0;
    const loop = (now: number) => {
      frame = requestAnimationFrame(loop);
      if (last && now - last < 34) return;
      const delta = last ? Math.min(now - last, 100) : 34;
      last = now;
      setTurns((current) => {
        const index = current.findIndex((turn) => turn.id === writingId);
        const turn = current[index];
        if (!turn || turn.phase !== "writing") return current;
        const next = (() => {
          const remaining = turn.source.length - turn.shown;
          if (remaining <= 0) return turn.sourceDone ? { ...turn, phase: "done" as TurnPhase } : turn;
          const rate = turn.streamed ? Math.max(480, remaining / 0.3) : 480;
          let shown = Math.min(turn.source.length, turn.shown + Math.max(12, Math.round((rate * delta) / 1000)));
          if (shown < turn.source.length) {
            const space = turn.source.slice(shown).search(/\s/);
            if (space >= 0) {
              shown += space;
              while (shown < turn.source.length && /\s/.test(turn.source[shown] ?? "")) shown += 1;
            } else if (turn.sourceDone) {
              shown = turn.source.length;
            } else {
              const before = turn.source.slice(0, shown);
              shown = Math.max(before.lastIndexOf(" "), before.lastIndexOf("\n")) + 1;
            }
          }
          if (shown <= turn.shown) return turn;
          const finished = shown >= turn.source.length && turn.sourceDone;
          return {
            ...turn,
            shown,
            chunks: [...turn.chunks, turn.shown],
            phase: finished ? ("done" as TurnPhase) : ("writing" as TurnPhase),
          };
        })();
        if (next === turn) return current;
        const copied = current.slice();
        copied[index] = next;
        return copied;
      });
    };
    frame = requestAnimationFrame(loop);
    return () => cancelAnimationFrame(frame);
  }, [writingId]);

  const currentPhase = lastTurn?.phase;
  useEffect(() => {
    if (currentPhase === "done") announce("Reply finished");
    else if (currentPhase === "stopped") announce("Reply stopped");
    else if (currentPhase === "error") announce("The reply did not finish");
  }, [currentPhase, announce]);

  useEffect(() => {
    const node = thread.current;
    const inner = node?.firstElementChild;
    if (!node || !inner) return;
    const observer = new ResizeObserver(() => {
      if (atBottom.current) node.scrollTop = node.scrollHeight;
    });
    observer.observe(inner);
    return () => observer.disconnect();
  }, [mode]);

  async function consume(id: number, stream: AsyncIterable<string>, signal: AbortSignal) {
    try {
      for await (const piece of stream) {
        if (signal.aborted) return;
        updateTurn(id, (turn) => ({
          ...turn,
          source: turn.source + piece,
          phase: turn.phase === "waiting" ? "writing" : turn.phase,
        }));
      }
      if (signal.aborted) return;
      updateTurn(id, (turn) => ({
        ...turn,
        sourceDone: true,
        phase: turn.phase === "waiting" ? "done" : turn.phase,
      }));
    } catch (error) {
      if (signal.aborted) return;
      updateTurn(id, (turn) => ({
        ...turn,
        phase: "error",
        sourceDone: true,
        shown: turn.source.length,
        error: error instanceof Error && error.message ? error.message : undefined,
      }));
    }
  }

  function send(message: SentMessage, replaceId?: number) {
    abort.current?.abort();
    const controller = new AbortController();
    abort.current = controller;
    let result: void | AsyncIterable<string>;
    try {
      result = onSend?.(message, { signal: controller.signal });
    } catch {
      result = undefined;
    }
    const stream =
      typeof result === "object" && result !== null && Symbol.asyncIterator in result ? result : null;
    const turnModel = models.find((option) => option.id === message.model) ?? model;
    const text = message.text.toLowerCase();
    const source = stream
      ? ""
      : text.startsWith("/summarize") || text.includes("launch plan")
        ? "The Q4 launch moves the self-serve tier to November 12, with the new pricing landing a week earlier on November 5. The main open risk is the checkout migration, which still needs a load test before the code freeze on October 30."
        : text.includes("launch post") || text.includes("pricing page")
          ? "Here is a draft: \u201cOur new pricing page is live. Every plan now shows exactly what you get, annual billing saves 20 percent, and you can switch plans any time without talking to sales.\u201d Want a shorter version for X?"
          : text.startsWith("/translate")
            ? "Hier ist die \u00dcbersetzung: Wir verschieben das Launch-Review auf Donnerstag, den 29. Oktober. Bitte bring die aktualisierten Preiszahlen mit."
            : text.startsWith("/fix")
              ? "The retry in submitPayment builds a new idempotency key on every attempt, so the processor treats each retry as a fresh charge. Create the key once per checkout and pass it to every retry, then cap retries at three with backoff."
              : text.startsWith("/explain") || text.includes("checkout")
                ? "When the first request times out, checkout.tsx retries the payment without the original idempotency key, so a slow network can produce a second charge. Reusing that key on every retry makes the processor return the first result instead of charging again."
                : text.includes("priya") || text.includes("reply") || text.includes("draft")
                  ? "Here is a draft: \u201cThanks for the detailed notes, Priya. We will keep the annual discount on the Team plan at 20 percent and revisit seat minimums after the November 12 review.\u201d"
                  : text.includes("thursday") || text.includes("october 29")
                    ? "I moved the launch review to Thursday, October 29 at 10:00 and sent Priya a note asking for the updated pricing numbers. The invite now links the Q4 launch plan so everyone opens the same version."
                    : "Here is where I would start: name the decision you need, then list the facts that support it and the questions still open. Add a file with the plus button or mention a doc with @, and I can work from the real details.";
    const turn: Turn = {
      id: nextTurnId.current++,
      message,
      model: turnModel,
      streamed: !!stream,
      phase: stream ? "waiting" : message.thinking ? "thinking" : message.webSearch ? "searching" : "waiting",
      source,
      sourceDone: !stream,
      shown: 0,
      chunks: [],
    };
    atBottom.current = true;
    setTurns((current) => (replaceId ? current.map((entry) => (entry.id === replaceId ? turn : entry)) : [...current, turn]));
    announce(stream ? `Sent. ${turnModel.name} is replying` : `Sent. Simulated reply from ${turnModel.name}`);
    if (stream) void consume(turn.id, stream, controller.signal);
  }

  function newChat() {
    abort.current?.abort();
    setTurns([]);
    announce("New chat");
    requestAnimationFrame(() => composer.current?.focus());
  }

  const acceptsFiles = (event: React.DragEvent) => Array.from(event.dataTransfer.types).includes("Files");

  return (
    <section
      className={cx(styles.root, className)}
      aria-label="Chat"
      onDragEnter={(event) => {
        if (!acceptsFiles(event)) return;
        event.preventDefault();
        dragDepth.current += 1;
        setDragging(true);
      }}
      onDragOver={(event) => {
        if (!acceptsFiles(event)) return;
        event.preventDefault();
        event.dataTransfer.dropEffect = "copy";
      }}
      onDragLeave={(event) => {
        if (!acceptsFiles(event)) return;
        dragDepth.current = Math.max(0, dragDepth.current - 1);
        if (!dragDepth.current) setDragging(false);
      }}
      onDrop={(event) => {
        if (!acceptsFiles(event)) return;
        event.preventDefault();
        dragDepth.current = 0;
        setDragging(false);
        composer.current?.addFiles(Array.from(event.dataTransfer.files));
        composer.current?.focus();
      }}
    >
      <AnimatePresence initial={false}>
        {mode === "thread" && (
          <motion.div
            key="new"
            className={styles.topBar}
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0, transition: { duration: duration.instant } }}
            transition={{ duration: duration.standard, delay: 0.2 * Number(!reduced) }}
          >
            <Tooltip content="New chat" side="left">
              <button type="button" className={styles.iconButton} aria-label="New chat" onClick={newChat}>
                <SquarePen aria-hidden="true" />
              </button>
            </Tooltip>
          </motion.div>
        )}
      </AnimatePresence>

      <div className={styles.stage} data-mode={mode}>
        {mode === "new" ? (
          <motion.div
            key="greeting"
            className={styles.intro}
            initial={reduced ? false : { opacity: 0, y: 8 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: duration.considered, ease: [...ease.enter] }}
          >
            <h2 className={styles.greeting}>{greeting}</h2>
          </motion.div>
        ) : (
          <div
            ref={thread}
            className={styles.thread}
            onScroll={(event) => {
              const node = event.currentTarget;
              atBottom.current = node.scrollHeight - node.scrollTop - node.clientHeight < 48;
            }}
          >
            <ol className={styles.turns}>
              {turns.map((turn, index) => (
                <li key={turn.id} className={styles.turn}>
                  <SentMessage turn={turn} reduced={reduced} />
                  <Reply
                    turn={turn}
                    last={index === turns.length - 1}
                    reduced={reduced}
                    onRetry={() => send(turn.message, turn.id)}
                  />
                </li>
              ))}
            </ol>
          </div>
        )}

        <motion.div
          className={styles.dock}
          layout={reduced ? false : "position"}
          layoutDependency={mode}
          transition={spring.smooth}
        >
          <Composer
            ref={composer}
            models={models}
            model={model}
            effort={effort}
            webSearch={webSearch}
            thinking={thinking}
            onModel={(id) => {
              const next = models.find((option) => option.id === id);
              if (!next) return;
              setModelId(id);
              announce(`${next.name} selected`);
            }}
            onEffort={(next) => {
              setEffort(next);
              announce(`${EFFORT_LABELS[next]} effort`);
            }}
            onTool={(tool) => {
              if (tool === "search") {
                setWebSearch((on) => !on);
                announce(webSearch ? "Web search off" : "Web search on");
              } else {
                setThinking((on) => !on);
                announce(thinking ? "Extended thinking off" : "Extended thinking on");
              }
            }}
            busy={busy}
            progress={progressInfo.progress}
            progressSeconds={progressInfo.seconds}
            run={lastTurn?.id ?? 0}
            placeholder={placeholder}
            maxLength={maxLength}
            reduced={reduced}
            onSubmit={(payload) =>
              send({
                ...payload,
                files: payload.attachments.map((item) => item.name),
                model: model.id,
                effort,
                webSearch,
                thinking,
              })
            }
            onStop={() => {
              if (!lastTurn || !working(lastTurn.phase)) return;
              abort.current?.abort();
              updateTurn(lastTurn.id, (turn) => ({ ...turn, phase: "stopped" }));
            }}
            announce={announce}
          />
        </motion.div>

        {mode === "new" && suggestions.length > 0 && (
          <motion.ul
            key="suggestions"
            className={styles.suggestions}
            aria-label="Suggestions"
            initial={reduced ? false : { opacity: 0, y: 6 }}
            animate={{ opacity: 1, y: 0 }}
            transition={{ duration: duration.considered, ease: [...ease.enter], delay: 0.06 * Number(!reduced) }}
          >
            {suggestions.slice(0, 3).map((suggestion) => {
              const Icon = suggestion.icon ? SUGGESTION_ICONS[suggestion.icon] : null;
              return (
                <li key={suggestion.id}>
                  <button
                    type="button"
                    className={styles.suggestion}
                    onClick={() => composer.current?.fill(suggestion.prompt)}
                  >
                    {Icon && <Icon className={styles.suggestionIcon} aria-hidden="true" />}
                    {suggestion.label}
                  </button>
                </li>
              );
            })}
          </motion.ul>
        )}
      </div>

      <AnimatePresence>
        {dragging && (
          <motion.div
            key="drop"
            className={styles.drop}
            initial={{ opacity: 0 }}
            animate={{ opacity: 1 }}
            exit={{ opacity: 0 }}
            transition={{ duration: duration.fast }}
          >
            <motion.span
              className={styles.dropInner}
              initial={reduced ? false : { y: 6 }}
              animate={{ y: 0 }}
              transition={reduced ? { duration: 0 } : settle}
            >
              <Upload className={styles.dropIcon} aria-hidden="true" />
              <span className={styles.dropTitle}>Drop files to attach</span>
              <span className={styles.dropNote}>PDFs, images, code and text, up to 30 MB each</span>
            </motion.span>
          </motion.div>
        )}
      </AnimatePresence>

      <p className={styles.srOnly} role="status" aria-live="polite">
        {announcement}
      </p>
    </section>
  );
}

export default AiComposer;
