"use client";

import {
  Fragment,
  memo,
  useCallback,
  useEffect,
  useId,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type ComponentType,
  type CSSProperties,
  type ReactNode,
} from "react";
import * as DropdownMenu from "@radix-ui/react-dropdown-menu";
import {
  AnimatePresence,
  animate,
  motion,
  useInView,
  useReducedMotion,
} from "motion/react";
import {
  Activity,
  ArrowDown,
  ArrowUp,
  Check,
  ChevronDown,
  ChevronLeft,
  ChevronRight,
  Code,
  Copy,
  Database,
  FileText,
  Globe,
  LoaderCircle,
  Paperclip,
  Pencil,
  RotateCcw,
  Search,
  Square,
  SquarePen,
  ThumbsDown,
  ThumbsUp,
  Timer,
  X,
} from "lucide-react";
import { TextShimmer } from "../text-shimmer/text-shimmer";
import { Tooltip } from "../tooltip/tooltip";
import { HoverCard } from "../hover-card/hover-card";
import { Button } from "../button/button";
import { CopyButton } from "../copy-button/copy-button";
import styles from "./ai-chat.module.css";

/* ------------------------------------------------------------------ types */

/** Props a model or source mark accepts, so any brand SVG can stand in for the samples. */
export interface ChatMarkProps {
  className?: string;
  style?: CSSProperties;
  title?: string;
  "aria-hidden"?: boolean | "true" | "false";
}

/** An inline SVG mark: the Claude starburst, the OpenAI knot, a Postgres elephant. */
export type ChatMark = ComponentType<ChatMarkProps>;

/** One model in the header menu. */
export interface ChatModel {
  /** Stable id passed back to `onModelChange` and forwarded to `respond`. */
  id: string;
  /** Shown next to the mark and read out in the chip's label. */
  name: string;
  /** One short line under the name in the menu, such as the provider. */
  note?: string;
  /** Any SVG component; the chat sizes it and inherits `currentColor`. */
  mark: ChatMark;
  /** Tints a mono mark instead of letting it inherit the text color. */
  color?: string;
}

/** A web source a reply cites, numbered by its position in the list. */
export interface ChatSource {
  title: string;
  url: string;
  snippet?: string;
  mark?: ChatMark;
  color?: string;
}

/** A tool call a reply made: a search, a query, a file read. */
export interface ChatTool {
  id: string;
  /** Past tense label on the settled chip, such as "Searched the web". */
  name: string;
  /** Present tense label while it runs, such as "Searching the web". */
  activeName?: string;
  status: "running" | "done" | "error";
  /** Short result after the name, such as "4 sources" or "40,213,880 rows". */
  summary?: string;
  icon?: "search" | "database" | "code" | "file";
  mark?: ChatMark;
  color?: string;
  /** Search queries, or the statements a query tool ran. */
  input?: string[];
  /** Result of a database or code tool, shown under its input. */
  output?: string;
  sources?: ChatSource[];
}

/** A file the person attached to their message. */
export interface ChatAttachment {
  id: string;
  name: string;
  size: number;
  type?: string;
}

export type ChatReplyStatus =
  "thinking" | "streaming" | "done" | "stopped" | "error";

/** One message in the conversation, in the shape `respond` receives. */
export interface AiChatMessage {
  id?: string;
  role: "user" | "assistant";
  content: string;
  attachments?: ChatAttachment[];
  /** Model id that wrote an assistant message. */
  model?: string;
  reasoning?: string;
  thoughtFor?: number;
  tools?: ChatTool[];
  sources?: ChatSource[];
  status?: ChatReplyStatus;
  error?: string;
}

/** An earlier conversation in the title menu. */
export interface AiChatHistoryItem {
  id: string;
  title: string;
  /** Menu heading it sits under, such as "Today". */
  group?: string;
  messages: AiChatMessage[];
}

/** A prompt offered in the empty state. */
export interface ChatSuggestion {
  label: string;
  prompt?: string;
  icon?: "database" | "timer" | "activity" | "search";
}

/** One event of a reply stream. Text can arrive in any chunk size; the chat paces the reveal. */
export type ChatStreamEvent =
  | { type: "reasoning"; delta: string }
  | { type: "reasoning-end"; seconds?: number }
  | { type: "tool"; tool: ChatTool }
  | { type: "sources"; sources: ChatSource[] }
  | { type: "text"; delta: string }
  | { type: "error"; message: string };

/** Streams one reply for the messages on screen. */
export type ChatRespond = (
  messages: AiChatMessage[],
  options: { signal: AbortSignal; model: ChatModel },
) => AsyncIterable<ChatStreamEvent>;

/** Props for {@link AiChat}. */
export interface AiChatProps {
  /** Streams one reply for the messages on screen. Yield reasoning, reasoning-end, tool, sources and text events; the reply ends when the iterable finishes. Throw or yield an error event to fail it with a Retry. Stop aborts the signal. Without it the chat answers with scripted, simulated replies. */
  respond?: ChatRespond;
  /** Models in the header menu. Defaults to Claude Opus 5.5, Claude Sonnet 5.5, GPT-6.1 Sol, Gemini 4 Argon and Mistral Large 4. */
  models?: ChatModel[];
  /** Id of the model selected first. Defaults to the first model. */
  defaultModel?: string;
  /** The open conversation, alternating user and assistant messages. A trailing user message is answered when the card first scrolls into view. */
  initialMessages?: AiChatMessage[];
  /** Title of the open conversation. Defaults to one taken from its first message. */
  title?: string;
  /** Earlier conversations in the title menu. Each opens settled at its latest message and keeps its own scroll position. */
  history?: AiChatHistoryItem[];
  /** Up to three prompts in the empty state. Defaults to the sample database, backfill and replica prompts. */
  suggestions?: ChatSuggestion[];
  /** Heading of the empty state. Defaults to "What are we working on?". */
  greeting?: string;
  /** Composer placeholder. Defaults to "Ask anything". */
  placeholder?: string;
  /** Plays a two turn scripted conversation once the card is in view. Defaults to true only without respond and initialMessages. Typing, sending or switching chats stops it. */
  autoPlay?: boolean;
  /** Called when a reply is rated or the rating is cleared. Without it the preview records the rating locally and announces it as simulated. */
  onFeedback?: (message: AiChatMessage, value: "up" | "down" | null) => void;
  /** Called when a model is picked. New replies use it; earlier replies keep the mark of the model that wrote them. */
  onModelChange?: (model: ChatModel) => void;
  /** Called by the New chat button and menu item. */
  onNewChat?: () => void;
  /** Class on the root. Set `--chat-height` to fit a smaller area and `--chat-column` for the width of the conversation column. */
  className?: string;
}

type ChatToolIcon = NonNullable<ChatTool["icon"]>;

interface ThreadReply {
  id: string;
  model: string;
  status: ChatReplyStatus;
  text: string;
  shown: number;
  streamDone: boolean;
  reasoning: string;
  thoughtFor?: number;
  tools: ChatTool[];
  sources: ChatSource[];
  feedback: "up" | "down" | null;
  error?: string;
  /** Offsets where a reveal chunk starts, so only new text animates in. */
  marks?: number[];
}

interface TurnVersion {
  id: string;
  prompt: string;
  attachments?: ChatAttachment[];
  reply: ThreadReply;
  next: string | null;
}

interface TurnRecord {
  id: string;
  versions: TurnVersion[];
  active: number;
}

interface Thread {
  root: string | null;
  turns: Record<string, TurnRecord>;
}

interface Chat {
  id: string;
  title: string;
  group: string;
  thread: Thread;
}

interface ActiveStream {
  controller: AbortController;
  chatId: string;
  turnId: string;
  versionId: string;
  started: number;
}

interface ChatActions {
  retry: (turnId: string) => void;
  edit: (turnId: string, prompt: string) => void;
  retryInPlace: (turnId: string, versionId: string) => void;
  select: (turnId: string, index: number) => void;
  feedback: (turnId: string, versionId: string, value: "up" | "down") => void;
  copied: (ok: boolean) => void;
}

/* ---------------------------------------------------------------- helpers */

const ENTER = [0.22, 1, 0.36, 1] as const;
const RESIZE = { type: "spring", visualDuration: 0.26, bounce: 0 } as const;
const FOLD = { type: "spring", visualDuration: 0.32, bounce: 0 } as const;

const cx = (...parts: Array<string | false | null | undefined>) =>
  parts.filter(Boolean).join(" ");

const subscribeHydration = () => () => {};
const clientSnapshot = () => true;
const serverSnapshot = () => false;

/** Reduced motion only applies once the page is hydrated, so the first paint matches the server. */
function useReduced(): boolean {
  const hydrated = useSyncExternalStore(
    subscribeHydration,
    clientSnapshot,
    serverSnapshot,
  );
  const prefersReduced = useReducedMotion();
  return Boolean(hydrated && prefersReduced);
}

const sleep = (ms: number, signal: AbortSignal) =>
  new Promise<void>((resolve) => {
    if (signal.aborted) return resolve();
    const timer = setTimeout(done, ms);
    function done() {
      clearTimeout(timer);
      signal.removeEventListener("abort", done);
      resolve();
    }
    signal.addEventListener("abort", done, { once: true });
  });

/** Greedy word-safe chunker, so a paced reveal never cuts a word in half. */
function chunkText(text: string, size: number): string[] {
  const out: string[] = [];
  let index = 0;
  while (index < text.length) {
    let end = Math.min(text.length, index + size);
    const space = text.indexOf(" ", end);
    end = space === -1 || space - end > 12 ? end : space + 1;
    out.push(text.slice(index, end));
    index = end;
  }
  return out;
}

const hostOf = (url: string) => {
  try {
    return new URL(url).hostname.replace(/^www\./, "");
  } catch {
    return url;
  }
};

/* ------------------------------------------------------------- primitives */

interface SwapProps {
  swapKey: string;
  children: ReactNode;
  reduced: boolean;
  className?: string;
}

/** Swaps its label with one crossfade while the box follows the measured width on a spring with no bounce. */
function Swap({ swapKey, children, reduced, className }: SwapProps) {
  const ref = useRef<HTMLSpanElement>(null);
  const rest = useRef(0);
  const lastKey = useRef(swapKey);

  useLayoutEffect(() => {
    const node = ref.current;
    if (!node || lastKey.current === swapKey) return;
    lastKey.current = swapKey;
    const from = node.style.width
      ? node.getBoundingClientRect().width
      : rest.current;
    node.style.width = "";
    const to = node.getBoundingClientRect().width;
    if (reduced || !from || Math.abs(from - to) < 1) return;
    const controls = animate(
      node,
      { width: [from, to] },
      {
        ...RESIZE,
        onComplete: () => {
          node.style.width = "";
        },
      },
    );
    return () => controls.stop();
  }, [swapKey, reduced]);

  // A width the page sets on its own (a late font, a parent reflow) becomes the next rest width.
  useEffect(() => {
    const node = ref.current;
    if (!node || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      if (!node.style.width) rest.current = node.getBoundingClientRect().width;
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  return (
    <span ref={ref} className={cx(styles.swap, className)}>
      <AnimatePresence mode="popLayout" initial={false}>
        <motion.span
          key={swapKey}
          className={styles.swapItem}
          initial={reduced ? { opacity: 0 } : { opacity: 0, y: 2 }}
          animate={{ opacity: 1, y: 0 }}
          exit={{
            opacity: 0,
            transition: { duration: reduced ? 0 : 0.1, ease: [...ENTER] },
          }}
          transition={{ duration: reduced ? 0 : 0.16, ease: [...ENTER] }}
        >
          {children}
        </motion.span>
      </AnimatePresence>
    </span>
  );
}

/** Crossfades one icon for another in place, without touching the layout. */
function IconSwap({ swapKey, children, reduced, className }: SwapProps) {
  return (
    <span className={cx(styles.iconCell, className)} aria-hidden="true">
      <AnimatePresence initial={false}>
        <motion.span
          key={swapKey}
          initial={{ opacity: 0 }}
          animate={{ opacity: 1 }}
          exit={{ opacity: 0, transition: { duration: reduced ? 0 : 0.1 } }}
          transition={{ duration: reduced ? 0 : 0.16, ease: [...ENTER] }}
        >
          {children}
        </motion.span>
      </AnimatePresence>
    </span>
  );
}

/** Glides a block to its new height when its key changes, so a new version or the editor never snaps. */
function useHeightFlip(
  ref: React.RefObject<HTMLElement | null>,
  key: string,
  reduced: boolean,
) {
  const rest = useRef(0);
  const lastKey = useRef(key);

  useEffect(() => {
    const node = ref.current;
    if (!node || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      if (!node.style.height)
        rest.current = node.getBoundingClientRect().height;
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, [ref]);

  useLayoutEffect(() => {
    const node = ref.current;
    if (!node || lastKey.current === key) return;
    lastKey.current = key;
    const from = node.style.height
      ? node.getBoundingClientRect().height
      : rest.current;
    node.style.height = "";
    const to = node.getBoundingClientRect().height;
    if (reduced || !from || Math.abs(from - to) < 2) return;
    node.style.overflow = "clip";
    const controls = animate(
      node,
      { height: [from, to] },
      {
        ...RESIZE,
        onComplete: () => {
          node.style.height = "";
          node.style.overflow = "";
        },
      },
    );
    return () => controls.stop();
  }, [key, reduced, ref]);
}

/* ------------------------------------------------------------------ marks */

function ClaudeMark({ title = "Claude", ...rest }: ChatMarkProps) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="currentColor"
      role="img"
      aria-label={title}
      {...rest}
    >
      <path d="m4.7144 15.9555 4.7174-2.6471.079-.2307-.079-.1275h-.2307l-.7893-.0486-2.6956-.0729-2.3375-.0971-2.2646-.1214-.5707-.1215-.5343-.7042.0546-.3522.4797-.3218.686.0608 1.5179.1032 2.2767.1578 1.6514.0972 2.4468.255h.3886l.0546-.1579-.1336-.0971-.1032-.0972L6.973 9.8356l-2.55-1.6879-1.3356-.9714-.7225-.4918-.3643-.4614-.1578-1.0078.6557-.7225.8803.0607.2246.0607.8925.686 1.9064 1.4754 2.4893 1.8336.3643.3035.1457-.1032.0182-.0728-.164-.2733-1.3539-2.4467-1.445-2.4893-.6435-1.032-.17-.6194c-.0607-.255-.1032-.4674-.1032-.7285L6.287.1335 6.6997 0l.9957.1336.419.3642.6192 1.4147 1.0018 2.2282 1.5543 3.0296.4553.8985.2429.8318.091.255h.1579v-.1457l.1275-1.706.2368-2.0947.2307-2.6957.0789-.7589.3764-.9107.7468-.4918.5828.2793.4797.686-.0668.4433-.2853 1.8517-.5586 2.9021-.3643 1.9429h.2125l.2429-.2429.9835-1.3053 1.6514-2.0643.7286-.8196.85-.9046.5464-.4311h1.0321l.759 1.1293-.34 1.1657-1.0625 1.3478-.8804 1.1414-1.2628 1.7-.7893 1.36.0729.1093.1882-.0183 2.8535-.607 1.5421-.2794 1.8396-.3157.8318.3886.091.3946-.3278.8075-1.967.4857-2.3072.4614-3.4364.8136-.0425.0304.0486.0607 1.5482.1457.6618.0364h1.621l3.0175.2247.7892.522.4736.6376-.079.4857-1.2142.6193-1.6393-.3886-3.825-.9107-1.3113-.3279h-.1822v.1093l1.0929 1.0686 2.0035 1.8092 2.5075 2.3314.1275.5768-.3218.4554-.34-.0486-2.2039-1.6575-.85-.7468-1.9246-1.621h-.1275v.17l.4432.6496 2.3436 3.5214.1214 1.0807-.17.3521-.6071.2125-.6679-.1214-1.3721-1.9246L14.38 17.959l-1.1414-1.9428-.1397.079-.674 7.2552-.3156.3703-.7286.2793-.6071-.4614-.3218-.7468.3218-1.4753.3886-1.9246.3157-1.53.2853-1.9004.17-.6314-.0121-.0425-.1397.0182-1.4328 1.9672-2.1796 2.9446-1.7243 1.8456-.4128.164-.7164-.3704.0667-.6618.4008-.5889 2.386-3.0357 1.4389-1.882.929-1.0868-.0062-.1579h-.0546l-6.3385 4.1164-1.1293.1457-.4857-.4554.0608-.7467.2307-.2429 1.9064-1.3114Z" />
    </svg>
  );
}

function OpenAIMark({ title = "OpenAI", ...rest }: ChatMarkProps) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="currentColor"
      role="img"
      aria-label={title}
      {...rest}
    >
      <path d="M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z" />
    </svg>
  );
}

function GeminiMark({ title = "Gemini", ...rest }: ChatMarkProps) {
  const uid = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  return (
    <svg viewBox="0 0 24 24" role="img" aria-label={title} {...rest}>
      <path
        d="M20.616 10.835a14.147 14.147 0 01-4.45-3.001 14.111 14.111 0 01-3.678-6.452.503.503 0 00-.975 0 14.134 14.134 0 01-3.679 6.452 14.155 14.155 0 01-4.45 3.001c-.65.28-1.318.505-2.002.678a.502.502 0 000 .975c.684.172 1.35.397 2.002.677a14.147 14.147 0 014.45 3.001 14.112 14.112 0 013.679 6.453.502.502 0 00.975 0c.172-.685.397-1.351.677-2.003a14.145 14.145 0 013.001-4.45 14.113 14.113 0 016.453-3.678.503.503 0 000-.975 13.245 13.245 0 01-2.003-.678z"
        fill="#3186FF"
      />
      <path
        d="M20.616 10.835a14.147 14.147 0 01-4.45-3.001 14.111 14.111 0 01-3.678-6.452.503.503 0 00-.975 0 14.134 14.134 0 01-3.679 6.452 14.155 14.155 0 01-4.45 3.001c-.65.28-1.318.505-2.002.678a.502.502 0 000 .975c.684.172 1.35.397 2.002.677a14.147 14.147 0 014.45 3.001 14.112 14.112 0 013.679 6.453.502.502 0 00.975 0c.172-.685.397-1.351.677-2.003a14.145 14.145 0 013.001-4.45 14.113 14.113 0 016.453-3.678.503.503 0 000-.975 13.245 13.245 0 01-2.003-.678z"
        fill={`url(#lobe-icons-gemini-0-${uid})`}
      />
      <path
        d="M20.616 10.835a14.147 14.147 0 01-4.45-3.001 14.111 14.111 0 01-3.678-6.452.503.503 0 00-.975 0 14.134 14.134 0 01-3.679 6.452 14.155 14.155 0 01-4.45 3.001c-.65.28-1.318.505-2.002.678a.502.502 0 000 .975c.684.172 1.35.397 2.002.677a14.147 14.147 0 014.45 3.001 14.112 14.112 0 013.679 6.453.502.502 0 00.975 0c.172-.685.397-1.351.677-2.003a14.145 14.145 0 013.001-4.45 14.113 14.113 0 016.453-3.678.503.503 0 000-.975 13.245 13.245 0 01-2.003-.678z"
        fill={`url(#lobe-icons-gemini-1-${uid})`}
      />
      <path
        d="M20.616 10.835a14.147 14.147 0 01-4.45-3.001 14.111 14.111 0 01-3.678-6.452.503.503 0 00-.975 0 14.134 14.134 0 01-3.679 6.452 14.155 14.155 0 01-4.45 3.001c-.65.28-1.318.505-2.002.678a.502.502 0 000 .975c.684.172 1.35.397 2.002.677a14.147 14.147 0 014.45 3.001 14.112 14.112 0 013.679 6.453.502.502 0 00.975 0c.172-.685.397-1.351.677-2.003a14.145 14.145 0 013.001-4.45 14.113 14.113 0 016.453-3.678.503.503 0 000-.975 13.245 13.245 0 01-2.003-.678z"
        fill={`url(#lobe-icons-gemini-2-${uid})`}
      />
      <defs>
        <linearGradient
          gradientUnits="userSpaceOnUse"
          id={`lobe-icons-gemini-0-${uid}`}
          x1="7"
          x2="11"
          y1="15.5"
          y2="12"
        >
          <stop stopColor="#08B962" />
          <stop offset="1" stopColor="#08B962" stopOpacity="0" />
        </linearGradient>
        <linearGradient
          gradientUnits="userSpaceOnUse"
          id={`lobe-icons-gemini-1-${uid}`}
          x1="8"
          x2="11.5"
          y1="5.5"
          y2="11"
        >
          <stop stopColor="#F94543" />
          <stop offset="1" stopColor="#F94543" stopOpacity="0" />
        </linearGradient>
        <linearGradient
          gradientUnits="userSpaceOnUse"
          id={`lobe-icons-gemini-2-${uid}`}
          x1="3.5"
          x2="17.5"
          y1="13.5"
          y2="12"
        >
          <stop stopColor="#FABC12" />
          <stop offset=".46" stopColor="#FABC12" stopOpacity="0" />
        </linearGradient>
      </defs>
    </svg>
  );
}

function MistralMark({ title = "Mistral AI", ...rest }: ChatMarkProps) {
  return (
    <svg viewBox="0 0 24 24" role="img" aria-label={title} {...rest}>
      <path
        d="M3.428 3.4h3.429v3.428H3.428V3.4zm13.714 0h3.43v3.428h-3.43V3.4z"
        fill="gold"
      />
      <path
        d="M3.428 6.828h6.857v3.429H3.429V6.828zm10.286 0h6.857v3.429h-6.857V6.828z"
        fill="#FFAF00"
      />
      <path d="M3.428 10.258h17.144v3.428H3.428v-3.428z" fill="#FF8205" />
      <path
        d="M3.428 13.686h3.429v3.428H3.428v-3.428zm6.858 0h3.429v3.428h-3.429v-3.428zm6.856 0h3.43v3.428h-3.43v-3.428z"
        fill="#FA500F"
      />
      <path
        d="M0 17.114h10.286v3.429H0v-3.429zm13.714 0H24v3.429H13.714v-3.429z"
        fill="#E10500"
      />
    </svg>
  );
}

function PostgresMark({ title = "PostgreSQL", ...rest }: ChatMarkProps) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="currentColor"
      role="img"
      aria-label={title}
      {...rest}
    >
      <path d="M23.5594 14.7228a.5269.5269 0 0 0-.0563-.1191c-.139-.2632-.4768-.3418-1.0074-.2321-1.6533.3411-2.2935.1312-2.5256-.0191 1.342-2.0482 2.445-4.522 3.0411-6.8297.2714-1.0507.7982-3.5237.1222-4.7316a1.5641 1.5641 0 0 0-.1509-.235C21.6931.9086 19.8007.0248 17.5099.0005c-1.4947-.0158-2.7705.3461-3.1161.4794a9.449 9.449 0 0 0-.5159-.0816 8.044 8.044 0 0 0-1.3114-.1278c-1.1822-.0184-2.2038.2642-3.0498.8406-.8573-.3211-4.7888-1.645-7.2219.0788C.9359 2.1526.3086 3.8733.4302 6.3043c.0409.818.5069 3.334 1.2423 5.7436.4598 1.5065.9387 2.7019 1.4334 3.582.553.9942 1.1259 1.5933 1.7143 1.7895.4474.1491 1.1327.1441 1.8581-.7279.8012-.9635 1.5903-1.8258 1.9446-2.2069.4351.2355.9064.3625 1.39.3772a.0569.0569 0 0 0 .0004.0041 11.0312 11.0312 0 0 0-.2472.3054c-.3389.4302-.4094.5197-1.5002.7443-.3102.064-1.1344.2339-1.1464.8115-.0025.1224.0329.2309.0919.3268.2269.4231.9216.6097 1.015.6331 1.3345.3335 2.5044.092 3.3714-.6787-.017 2.231.0775 4.4174.3454 5.0874.2212.5529.7618 1.9045 2.4692 1.9043.2505 0 .5263-.0291.8296-.0941 1.7819-.3821 2.5557-1.1696 2.855-2.9059.1503-.8707.4016-2.8753.5388-4.1012.0169-.0703.0357-.1207.057-.1362.0007-.0005.0697-.0471.4272.0307a.3673.3673 0 0 0 .0443.0068l.2539.0223.0149.001c.8468.0384 1.9114-.1426 2.5312-.4308.6438-.2988 1.8057-1.0323 1.5951-1.6698zM2.371 11.8765c-.7435-2.4358-1.1779-4.8851-1.2123-5.5719-.1086-2.1714.4171-3.6829 1.5623-4.4927 1.8367-1.2986 4.8398-.5408 6.108-.13-.0032.0032-.0066.0061-.0098.0094-2.0238 2.044-1.9758 5.536-1.9708 5.7495-.0002.0823.0066.1989.0162.3593.0348.5873.0996 1.6804-.0735 2.9184-.1609 1.1504.1937 2.2764.9728 3.0892.0806.0841.1648.1631.2518.2374-.3468.3714-1.1004 1.1926-1.9025 2.1576-.5677.6825-.9597.5517-1.0886.5087-.3919-.1307-.813-.5871-1.2381-1.3223-.4796-.839-.9635-2.0317-1.4155-3.5126zm6.0072 5.0871c-.1711-.0428-.3271-.1132-.4322-.1772.0889-.0394.2374-.0902.4833-.1409 1.2833-.2641 1.4815-.4506 1.9143-1.0002.0992-.126.2116-.2687.3673-.4426a.3549.3549 0 0 0 .0737-.1298c.1708-.1513.2724-.1099.4369-.0417.156.0646.3078.26.3695.4752.0291.1016.0619.2945-.0452.4444-.9043 1.2658-2.2216 1.2494-3.1676 1.0128zm2.094-3.988-.0525.141c-.133.3566-.2567.6881-.3334 1.003-.6674-.0021-1.3168-.2872-1.8105-.8024-.6279-.6551-.9131-1.5664-.7825-2.5004.1828-1.3079.1153-2.4468.079-3.0586-.005-.0857-.0095-.1607-.0122-.2199.2957-.2621 1.6659-.9962 2.6429-.7724.4459.1022.7176.4057.8305.928.5846 2.7038.0774 3.8307-.3302 4.7363-.084.1866-.1633.3629-.2311.5454zm7.3637 4.5725c-.0169.1768-.0358.376-.0618.5959l-.146.4383a.3547.3547 0 0 0-.0182.1077c-.0059.4747-.054.6489-.115.8693-.0634.2292-.1353.4891-.1794 1.0575-.11 1.4143-.8782 2.2267-2.4172 2.5565-1.5155.3251-1.7843-.4968-2.0212-1.2217a6.5824 6.5824 0 0 0-.0769-.2266c-.2154-.5858-.1911-1.4119-.1574-2.5551.0165-.5612-.0249-1.9013-.3302-2.6462.0044-.2932.0106-.5909.019-.8918a.3529.3529 0 0 0-.0153-.1126 1.4927 1.4927 0 0 0-.0439-.208c-.1226-.4283-.4213-.7866-.7797-.9351-.1424-.059-.4038-.1672-.7178-.0869.067-.276.1831-.5875.309-.9249l.0529-.142c.0595-.16.134-.3257.213-.5012.4265-.9476 1.0106-2.2453.3766-5.1772-.2374-1.0981-1.0304-1.6343-2.2324-1.5098-.7207.0746-1.3799.3654-1.7088.5321a5.6716 5.6716 0 0 0-.1958.1041c.0918-1.1064.4386-3.1741 1.7357-4.4823a4.0306 4.0306 0 0 1 .3033-.276.3532.3532 0 0 0 .1447-.0644c.7524-.5706 1.6945-.8506 2.802-.8325.4091.0067.8017.0339 1.1742.081 1.939.3544 3.2439 1.4468 4.0359 2.3827.8143.9623 1.2552 1.9315 1.4312 2.4543-1.3232-.1346-2.2234.1268-2.6797.779-.9926 1.4189.543 4.1729 1.2811 5.4964.1353.2426.2522.4522.2889.5413.2403.5825.5515.9713.7787 1.2552.0696.087.1372.1714.1885.245-.4008.1155-1.1208.3825-1.0552 1.717-.0123.1563-.0423.4469-.0834.8148-.0461.2077-.0702.4603-.0994.7662zm.8905-1.6211c-.0405-.8316.2691-.9185.5967-1.0105a2.8566 2.8566 0 0 0 .135-.0406 1.202 1.202 0 0 0 .1342.103c.5703.3765 1.5823.4213 3.0068.1344-.2016.1769-.5189.3994-.9533.6011-.4098.1903-1.0957.333-1.7473.3636-.7197.0336-1.0859-.0807-1.1721-.151zm.5695-9.2712c-.0059.3508-.0542.6692-.1054 1.0017-.055.3576-.112.7274-.1264 1.1762-.0142.4368.0404.8909.0932 1.3301.1066.887.216 1.8003-.2075 2.7014a3.5272 3.5272 0 0 1-.1876-.3856c-.0527-.1276-.1669-.3326-.3251-.6162-.6156-1.1041-2.0574-3.6896-1.3193-4.7446.3795-.5427 1.3408-.5661 2.1781-.463zm.2284 7.0137a12.3762 12.3762 0 0 0-.0853-.1074l-.0355-.0444c.7262-1.1995.5842-2.3862.4578-3.4385-.0519-.4318-.1009-.8396-.0885-1.2226.0129-.4061.0666-.7543.1185-1.0911.0639-.415.1288-.8443.1109-1.3505.0134-.0531.0188-.1158.0118-.1902-.0457-.4855-.5999-1.938-1.7294-3.253-.6076-.7073-1.4896-1.4972-2.6889-2.0395.5251-.1066 1.2328-.2035 2.0244-.1859 2.0515.0456 3.6746.8135 4.8242 2.2824a.908.908 0 0 1 .0667.1002c.7231 1.3556-.2762 6.2751-2.9867 10.5405zm-8.8166-6.1162c-.025.1794-.3089.4225-.6211.4225a.5821.5821 0 0 1-.0809-.0056c-.1873-.026-.3765-.144-.5059-.3156-.0458-.0605-.1203-.178-.1055-.2844.0055-.0401.0261-.0985.0925-.1488.1182-.0894.3518-.1226.6096-.0867.3163.0441.6426.1938.6113.4186zm7.9305-.4114c.0111.0792-.049.201-.1531.3102-.0683.0717-.212.1961-.4079.2232a.5456.5456 0 0 1-.075.0052c-.2935 0-.5414-.2344-.5607-.3717-.024-.1765.2641-.3106.5611-.352.297-.0414.6111.0088.6356.1851z" />
    </svg>
  );
}

function GitHubMark({ title = "GitHub", ...rest }: ChatMarkProps) {
  return (
    <svg
      viewBox="0 0 24 24"
      fill="currentColor"
      role="img"
      aria-label={title}
      {...rest}
    >
      <path d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" />
    </svg>
  );
}

function ModelMark({
  model,
  className,
}: {
  model: ChatModel;
  className?: string;
}) {
  const Mark = model.mark;
  return (
    <Mark
      className={cx(styles.mark, className)}
      style={model.color ? { color: model.color } : undefined}
      aria-hidden="true"
      title=""
    />
  );
}

function SourceMark({ source }: { source: ChatSource }) {
  const Mark = source.mark;
  return Mark ? (
    <Mark
      className={styles.mark}
      style={source.color ? { color: source.color } : undefined}
      aria-hidden="true"
      title=""
    />
  ) : (
    <Globe
      className={styles.mark}
      size={14}
      strokeWidth={1.75}
      aria-hidden="true"
    />
  );
}

/* ------------------------------------------------------------------ chips */

function ModelPicker({
  models,
  value,
  onChange,
  reduced,
}: {
  models: ChatModel[];
  value: ChatModel;
  onChange: (id: string) => void;
  reduced: boolean;
}) {
  return (
    <DropdownMenu.Root modal={false}>
      <DropdownMenu.Trigger
        className={styles.modelChip}
        aria-label={`Model: ${value.name}`}
      >
        <Swap swapKey={value.id} reduced={reduced}>
          <ModelMark model={value} />
          <span className={styles.modelName}>{value.name}</span>
        </Swap>
        <ChevronDown
          className={styles.chipChevron}
          size={14}
          strokeWidth={1.75}
          aria-hidden="true"
        />
      </DropdownMenu.Trigger>
      <DropdownMenu.Portal>
        <DropdownMenu.Content
          className={styles.menu}
          align="end"
          sideOffset={6}
          collisionPadding={12}
          loop
        >
          <DropdownMenu.RadioGroup value={value.id} onValueChange={onChange}>
            {models.map((model) => (
              <DropdownMenu.RadioItem
                key={model.id}
                value={model.id}
                className={styles.menuItem}
              >
                <ModelMark model={model} />
                <span className={styles.menuName}>{model.name}</span>
                {model.note ? (
                  <span className={styles.menuNote}>{model.note}</span>
                ) : null}
                <span className={styles.menuCheck}>
                  <DropdownMenu.ItemIndicator>
                    <Check size={15} strokeWidth={1.75} aria-hidden="true" />
                  </DropdownMenu.ItemIndicator>
                </span>
              </DropdownMenu.RadioItem>
            ))}
          </DropdownMenu.RadioGroup>
        </DropdownMenu.Content>
      </DropdownMenu.Portal>
    </DropdownMenu.Root>
  );
}

function ActionButton({
  label,
  onClick,
  children,
  pressed,
  disabled,
  className,
}: {
  label: string;
  onClick: () => void;
  children: ReactNode;
  pressed?: boolean;
  disabled?: boolean;
  className?: string;
}) {
  return (
    <Tooltip content={label} side="bottom">
      <button
        type="button"
        className={cx(styles.action, className)}
        aria-label={label}
        aria-pressed={pressed}
        disabled={disabled}
        onClick={onClick}
      >
        {children}
      </button>
    </Tooltip>
  );
}

async function copyText(text: string) {
  if (navigator.clipboard?.writeText)
    return navigator.clipboard.writeText(text);
  const area = document.createElement("textarea");
  area.value = text;
  area.setAttribute("readonly", "");
  area.style.position = "fixed";
  area.style.opacity = "0";
  document.body.append(area);
  area.select();
  const ok = document.execCommand("copy");
  area.remove();
  if (!ok) throw new Error("copy failed");
}

/** Copy action whose icon swaps to a check or a cross for 1.6s, then settles back. */
function CopyAction({
  text,
  label = "Copy",
  reduced,
  onCopied,
}: {
  text: string;
  label?: string;
  reduced: boolean;
  onCopied?: (ok: boolean) => void;
}) {
  const [state, setState] = useState<"idle" | "copied" | "error">("idle");
  const timer = useRef(0);

  async function run() {
    let ok = true;
    try {
      await copyText(text);
    } catch {
      ok = false;
    }
    setState(ok ? "copied" : "error");
    onCopied?.(ok);
    window.clearTimeout(timer.current);
    timer.current = window.setTimeout(() => setState("idle"), 1600);
  }

  useEffect(() => () => window.clearTimeout(timer.current), []);

  return (
    <ActionButton label={label} onClick={() => void run()}>
      <IconSwap swapKey={state} reduced={reduced} className={styles.copyIcon}>
        <span data-state={state}>
          {state === "copied" ? (
            <Check size={16} strokeWidth={1.75} />
          ) : state === "error" ? (
            <X size={16} strokeWidth={1.75} />
          ) : (
            <Copy size={16} strokeWidth={1.75} />
          )}
        </span>
      </IconSwap>
    </ActionButton>
  );
}

/* -------------------------------------------------------------- reasoning */

function Reasoning({
  model,
  reasoning,
  thinking,
  thoughtFor,
  reduced,
}: {
  model: ChatModel;
  reasoning: string;
  thinking: boolean;
  thoughtFor?: number;
  reduced: boolean;
}) {
  const [open, setOpen] = useState(false);
  const id = useId();
  const label = thinking
    ? "Thinking"
    : thoughtFor
      ? `Thought for ${thoughtFor}s`
      : reasoning
        ? "Thought briefly"
        : model.name;
  const expandable = Boolean(reasoning) && (!thinking || Boolean(reasoning));

  return (
    <div className={styles.reasoning}>
      <div className={styles.replyHead}>
        <Tooltip content={model.name}>
          <span className={styles.replyMark}>
            <ModelMark model={model} />
          </span>
        </Tooltip>
        <button
          type="button"
          className={styles.thoughtButton}
          aria-expanded={expandable ? open : undefined}
          aria-controls={open ? id : undefined}
          disabled={!expandable}
          onClick={() => setOpen((value) => !value)}
        >
          <Swap swapKey={thinking ? "thinking" : label} reduced={reduced}>
            {thinking ? <TextShimmer>Thinking</TextShimmer> : label}
          </Swap>
          <ChevronRight
            className={styles.thoughtChevron}
            data-open={open || undefined}
            data-hidden={!expandable || undefined}
            size={14}
            strokeWidth={1.75}
            aria-hidden="true"
          />
        </button>
      </div>
      <AnimatePresence initial={false}>
        {open && reasoning ? (
          <motion.div
            key="thoughts"
            id={id}
            className={styles.clip}
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: "auto", opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={
              reduced
                ? { duration: 0 }
                : {
                    height: FOLD,
                    opacity: { duration: 0.16, ease: [...ENTER] },
                  }
            }
          >
            <p className={styles.thoughtText}>{reasoning}</p>
          </motion.div>
        ) : null}
      </AnimatePresence>
    </div>
  );
}

/* ------------------------------------------------------------------ tools */

const TOOL_ICONS = {
  search: Search,
  database: Database,
  code: Code,
  file: FileText,
} as const;

function ToolChip({
  tool,
  open,
  onToggle,
  detailId,
  reduced,
}: {
  tool: ChatTool;
  open: boolean;
  onToggle: () => void;
  detailId: string;
  reduced: boolean;
}) {
  const running = tool.status === "running";
  const Icon = TOOL_ICONS[tool.icon ?? "search"];
  const Mark = tool.mark;
  const expandable =
    !running &&
    Boolean(tool.input?.length || tool.sources?.length || tool.output);
  const domains = new Set<string>();
  const marks = (tool.sources ?? []).filter((source) => {
    const host = hostOf(source.url);
    if (domains.has(host)) return false;
    domains.add(host);
    return true;
  });

  return (
    <button
      type="button"
      className={styles.toolChip}
      data-status={tool.status}
      aria-expanded={expandable ? open : undefined}
      aria-controls={open ? detailId : undefined}
      disabled={!expandable}
      onClick={onToggle}
    >
      <IconSwap
        swapKey={tool.status}
        reduced={reduced}
        className={styles.toolIcon}
      >
        {running ? (
          <LoaderCircle className={styles.spin} size={14} strokeWidth={1.75} />
        ) : tool.status === "error" ? (
          <X size={14} strokeWidth={1.75} />
        ) : Mark ? (
          <Mark
            className={styles.mark}
            style={tool.color ? { color: tool.color } : undefined}
            title=""
          />
        ) : (
          <Icon size={14} strokeWidth={1.75} />
        )}
      </IconSwap>
      <Swap
        swapKey={running ? "running" : `${tool.status}-${tool.name}`}
        reduced={reduced}
      >
        {running ? (
          <TextShimmer>{tool.activeName ?? tool.name}</TextShimmer>
        ) : (
          <>
            <span className={styles.toolName}>{tool.name}</span>
            {marks.length > 0 ? (
              <span className={styles.toolMarks}>
                {marks.slice(0, 3).map((source) => (
                  <SourceMark key={source.url} source={source} />
                ))}
              </span>
            ) : null}
            {tool.summary ? (
              <span className={styles.toolSummary}>{tool.summary}</span>
            ) : null}
          </>
        )}
      </Swap>
    </button>
  );
}

function ToolDetail({ tool }: { tool: ChatTool }) {
  const statements = tool.icon === "database" || tool.icon === "code";

  if (statements) {
    return (
      <div className={styles.toolDetail}>
        {tool.input?.map((line) => (
          <pre key={line} className={styles.toolCode}>
            {line}
          </pre>
        ))}
        {tool.output ? (
          <p className={styles.toolOutput}>
            <span>Result</span>
            <span className={styles.toolOutputValue}>{tool.output}</span>
          </p>
        ) : null}
      </div>
    );
  }

  return (
    <div className={styles.toolDetail}>
      {tool.input && tool.input.length > 0 ? (
        <ul className={styles.queries} aria-label="Searches">
          {tool.input.map((query) => (
            <li key={query}>
              <Search size={13} strokeWidth={1.75} aria-hidden="true" />
              {query}
            </li>
          ))}
        </ul>
      ) : null}
      {tool.sources && tool.sources.length > 0 ? (
        <ol className={styles.sourceList} aria-label="Sources">
          {tool.sources.map((source, index) => (
            <li key={`${source.url}${index}`}>
              <a
                href={source.url}
                target="_blank"
                rel="noreferrer"
                className={styles.sourceRow}
              >
                <SourceMark source={source} />
                <span className={styles.sourceRowTitle}>{source.title}</span>
                <span className={styles.sourceRowDomain}>
                  {hostOf(source.url)}
                </span>
              </a>
            </li>
          ))}
        </ol>
      ) : null}
    </div>
  );
}

function ToolList({ tools, reduced }: { tools: ChatTool[]; reduced: boolean }) {
  const [openId, setOpenId] = useState<string | null>(null);
  const id = useId();
  const open = tools.find(
    (tool) => tool.id === openId && tool.status !== "running",
  );

  return (
    <div className={styles.tools}>
      <div className={styles.toolRow}>
        {tools.map((tool) => (
          <ToolChip
            key={tool.id}
            tool={tool}
            open={openId === tool.id}
            detailId={id}
            reduced={reduced}
            onToggle={() =>
              setOpenId((current) => (current === tool.id ? null : tool.id))
            }
          />
        ))}
      </div>
      <AnimatePresence initial={false}>
        {open ? (
          <motion.div
            key="detail"
            id={id}
            className={styles.clip}
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: "auto", opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={
              reduced
                ? { duration: 0 }
                : {
                    height: FOLD,
                    opacity: { duration: 0.16, ease: [...ENTER] },
                  }
            }
          >
            <ToolDetail tool={open} />
          </motion.div>
        ) : null}
      </AnimatePresence>
    </div>
  );
}

function Versions({
  index,
  total,
  onChange,
  disabled,
}: {
  index: number;
  total: number;
  onChange: (index: number) => void;
  disabled?: boolean;
}) {
  return (
    <div
      className={styles.versions}
      role="group"
      aria-label={`Version ${index + 1} of ${total}`}
    >
      <button
        type="button"
        className={styles.versionButton}
        aria-label="Previous version"
        disabled={disabled || index === 0}
        onClick={() => onChange(index - 1)}
      >
        <ChevronLeft size={15} strokeWidth={1.75} aria-hidden="true" />
      </button>
      <span className={styles.versionCount} aria-hidden="true">
        {index + 1} / {total}
      </span>
      <button
        type="button"
        className={styles.versionButton}
        aria-label="Next version"
        disabled={disabled || index === total - 1}
        onClick={() => onChange(index + 1)}
      >
        <ChevronRight size={15} strokeWidth={1.75} aria-hidden="true" />
      </button>
    </div>
  );
}

/* --------------------------------------------------------------- composer */

function formatSize(bytes: number) {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${Math.round(bytes / 1024)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

function Composer({
  value,
  onChange,
  onSubmit,
  onStop,
  busy,
  placeholder,
  attachments,
  onAttach,
  onRemove,
  inputRef,
  reduced,
}: {
  value: string;
  onChange: (value: string) => void;
  onSubmit: () => void;
  onStop: () => void;
  busy: boolean;
  placeholder: string;
  attachments: ChatAttachment[];
  onAttach: (files: File[]) => void;
  onRemove: (id: string) => void;
  inputRef: React.RefObject<HTMLTextAreaElement | null>;
  reduced: boolean;
}) {
  const fileRef = useRef<HTMLInputElement>(null);
  const mirrorRef = useRef<HTMLDivElement>(null);
  const [fieldHeight, setFieldHeight] = useState(36);

  useLayoutEffect(() => {
    const mirror = mirrorRef.current;
    if (mirror) setFieldHeight(Math.min(mirror.offsetHeight, 180));
  }, [value]);

  useEffect(() => {
    const mirror = mirrorRef.current;
    if (!mirror || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() =>
      setFieldHeight(Math.min(mirror.offsetHeight, 180)),
    );
    observer.observe(mirror);
    return () => observer.disconnect();
  }, []);

  const hasContent = Boolean(value.trim()) || attachments.length > 0;

  function submit(event?: { preventDefault: () => void }) {
    event?.preventDefault();
    if (busy) onStop();
    else if (hasContent) onSubmit();
  }

  return (
    <form className={styles.composer} onSubmit={submit}>
      <AnimatePresence initial={false}>
        {attachments.length > 0 ? (
          <motion.div
            key="files"
            className={styles.clip}
            initial={{ height: 0, opacity: 0 }}
            animate={{ height: "auto", opacity: 1 }}
            exit={{ height: 0, opacity: 0 }}
            transition={
              reduced
                ? { duration: 0 }
                : {
                    height: RESIZE,
                    opacity: { duration: 0.16, ease: [...ENTER] },
                  }
            }
          >
            <ul className={styles.attachRow} aria-label="Attachments">
              {attachments.map((file) => (
                <li key={file.id} className={styles.attachChip}>
                  <FileText size={14} strokeWidth={1.75} aria-hidden="true" />
                  <span className={styles.attachName}>{file.name}</span>
                  <span className={styles.attachSize}>
                    {formatSize(file.size)}
                  </span>
                  <button
                    type="button"
                    className={styles.attachRemove}
                    aria-label={`Remove ${file.name}`}
                    onClick={() => onRemove(file.id)}
                  >
                    <X size={13} strokeWidth={1.75} aria-hidden="true" />
                  </button>
                </li>
              ))}
            </ul>
          </motion.div>
        ) : null}
      </AnimatePresence>

      <div className={styles.composerRow}>
        <Tooltip content="Attach files">
          <button
            type="button"
            className={styles.roundButton}
            aria-label="Attach files"
            onClick={() => fileRef.current?.click()}
          >
            <Paperclip size={18} strokeWidth={1.75} aria-hidden="true" />
          </button>
        </Tooltip>
        <input
          ref={fileRef}
          type="file"
          multiple
          hidden
          tabIndex={-1}
          onChange={(event) => {
            const files = Array.from(event.target.files ?? []);
            if (files.length) onAttach(files);
            event.target.value = "";
          }}
        />
        <div className={styles.field} style={{ height: fieldHeight }}>
          <div ref={mirrorRef} className={styles.mirror} aria-hidden="true">
            {value + "\u200b"}
          </div>
          <textarea
            ref={inputRef}
            className={styles.input}
            value={value}
            rows={1}
            placeholder={placeholder}
            aria-label="Message"
            onChange={(event) => onChange(event.target.value)}
            onKeyDown={(event) => {
              if (
                event.key === "Enter" &&
                !event.shiftKey &&
                !event.nativeEvent.isComposing
              ) {
                event.preventDefault();
                if (!busy) submit();
              }
              if (event.key === "Escape" && busy) {
                event.preventDefault();
                onStop();
              }
            }}
          />
        </div>
        <button
          type="submit"
          className={styles.send}
          data-busy={busy || undefined}
          aria-label={busy ? "Stop reply" : "Send message"}
          disabled={!busy && !hasContent}
        >
          <IconSwap swapKey={busy ? "stop" : "send"} reduced={reduced}>
            {busy ? (
              <Square size={13} strokeWidth={0} fill="currentColor" />
            ) : (
              <ArrowUp size={18} strokeWidth={2} />
            )}
          </IconSwap>
        </button>
      </div>
    </form>
  );
}

/* -------------------------------------------------------------- user turn */

function UserRow({
  turnId,
  version,
  busy,
  reduced,
  actions,
}: {
  turnId: string;
  version: TurnVersion;
  busy: boolean;
  reduced: boolean;
  actions: ChatActions;
}) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(version.prompt);
  const ref = useRef<HTMLDivElement>(null);
  useHeightFlip(ref, editing ? "edit" : version.id, reduced);

  function cancel() {
    setEditing(false);
    setDraft(version.prompt);
  }

  function save(event?: { preventDefault: () => void }) {
    event?.preventDefault();
    const next = draft.trim();
    if (!next) return;
    setEditing(false);
    if (next !== version.prompt.trim()) actions.edit(turnId, next);
  }

  return (
    <div ref={ref} className={styles.userRow}>
      {editing ? (
        <form className={cx(styles.editor, styles.fadeIn)} onSubmit={save}>
          <textarea
            className={styles.editorInput}
            value={draft}
            aria-label="Edit message"
            autoFocus
            rows={Math.min(
              8,
              Math.max(
                2,
                draft.split("\n").length + Math.floor(draft.length / 70),
              ),
            )}
            onChange={(event) => setDraft(event.target.value)}
            onKeyDown={(event) => {
              if (
                event.key === "Enter" &&
                !event.shiftKey &&
                !event.nativeEvent.isComposing
              ) {
                event.preventDefault();
                save();
              }
              if (event.key === "Escape") {
                event.preventDefault();
                cancel();
              }
            }}
            onFocus={(event) => {
              const end = event.currentTarget.value.length;
              event.currentTarget.setSelectionRange(end, end);
            }}
          />
          <div className={styles.editorBar}>
            <Button type="button" variant="ghost" size="sm" onClick={cancel}>
              Cancel
            </Button>
            <Button
              type="submit"
              variant="primary"
              size="sm"
              disabled={!draft.trim()}
            >
              Send
            </Button>
          </div>
        </form>
      ) : (
        <>
          <div className={cx(styles.userActions, styles.hoverable)}>
            <CopyAction
              text={version.prompt}
              reduced={reduced}
              onCopied={actions.copied}
            />
            <ActionButton
              label="Edit"
              disabled={busy}
              onClick={() => {
                setDraft(version.prompt);
                setEditing(true);
              }}
            >
              <Pencil size={15} strokeWidth={1.75} aria-hidden="true" />
            </ActionButton>
          </div>
          <div className={styles.bubble}>
            {version.attachments && version.attachments.length > 0 ? (
              <ul className={styles.bubbleFiles} aria-label="Attachments">
                {version.attachments.map((file) => (
                  <li key={file.id}>
                    <FileText size={14} strokeWidth={1.75} aria-hidden="true" />
                    {file.name}
                  </li>
                ))}
              </ul>
            ) : null}
            {version.prompt ? (
              <p className={styles.bubbleText}>
                {version.prompt.split(/(`[^`\n]+`)/g).map((part, index) =>
                  part.startsWith("`") &&
                  part.endsWith("`") &&
                  part.length > 2 ? (
                    <code key={index} className={styles.inlineCode}>
                      {part.slice(1, -1)}
                    </code>
                  ) : (
                    part
                  ),
                )}
              </p>
            ) : null}
          </div>
        </>
      )}
    </div>
  );
}

/* ----------------------------------------------------------- assistant turn */

function ReplyView({
  turn,
  version,
  model,
  busy,
  reduced,
  actions,
}: {
  turn: TurnRecord;
  version: TurnVersion;
  model: ChatModel;
  busy: boolean;
  reduced: boolean;
  actions: ChatActions;
}) {
  const reply = version.reply;
  const live = reply.status === "thinking" || reply.status === "streaming";
  const thinking =
    reply.status === "thinking" &&
    reply.thoughtFor === undefined &&
    !reply.text;
  const settled = reply.status === "done" || reply.status === "stopped";
  const visible = reply.text.slice(0, reply.shown);

  return (
    <div
      className={styles.reply}
      aria-busy={live || undefined}
      data-pending={live || undefined}
    >
      <Reasoning
        key={reply.id}
        model={model}
        reasoning={reply.reasoning}
        thinking={thinking}
        thoughtFor={reply.thoughtFor}
        reduced={reduced}
      />
      {reply.tools.length > 0 ? (
        <ToolList
          key={`tools-${reply.id}`}
          tools={reply.tools}
          reduced={reduced}
        />
      ) : null}
      {reply.shown > 0 ? (
        <Markdown
          source={reply.text}
          shown={reply.shown}
          marks={reply.marks}
          sources={reply.sources}
        />
      ) : null}
      {reply.status === "stopped" ? (
        <p className={cx(styles.statusLine, styles.fadeIn)}>
          <Square
            size={11}
            strokeWidth={0}
            fill="currentColor"
            aria-hidden="true"
          />
          {reply.shown ? "Stopped" : "Stopped before answering"}
        </p>
      ) : null}
      {reply.status === "error" ? (
        <div className={cx(styles.error, styles.fadeIn)}>
          <X
            className={styles.errorIcon}
            size={16}
            strokeWidth={1.75}
            aria-hidden="true"
          />
          <p className={styles.errorText}>
            <span className={styles.errorTitle}>Reply failed.</span>{" "}
            {reply.error}
          </p>
          <Button
            variant="secondary"
            size="sm"
            onClick={() => actions.retryInPlace(turn.id, version.id)}
          >
            Retry
          </Button>
        </div>
      ) : null}
      {reply.status !== "error" ? (
        <div className={styles.actions} data-hidden={!settled || undefined}>
          {turn.versions.length > 1 ? (
            <Versions
              index={turn.active}
              total={turn.versions.length}
              disabled={busy}
              onChange={(index) => actions.select(turn.id, index)}
            />
          ) : null}
          <div className={cx(styles.actionGroup, styles.hoverable)}>
            <CopyAction
              text={visible
                .replace(/ ?\[\d{1,2}\]/g, "")
                .replace(/\*\*/g, "")
                .replace(/`(?!``)/g, "")
                .trim()}
              reduced={reduced}
              onCopied={actions.copied}
            />
            <ActionButton label="Retry" onClick={() => actions.retry(turn.id)}>
              <RotateCcw size={15} strokeWidth={1.75} aria-hidden="true" />
            </ActionButton>
            <ActionButton
              label="Good response"
              pressed={reply.feedback === "up"}
              onClick={() => actions.feedback(turn.id, version.id, "up")}
            >
              <ThumbsUp
                size={15}
                strokeWidth={1.75}
                fill={reply.feedback === "up" ? "currentColor" : "none"}
                aria-hidden="true"
              />
            </ActionButton>
            <ActionButton
              label="Bad response"
              pressed={reply.feedback === "down"}
              onClick={() => actions.feedback(turn.id, version.id, "down")}
            >
              <ThumbsDown
                size={15}
                strokeWidth={1.75}
                fill={reply.feedback === "down" ? "currentColor" : "none"}
                aria-hidden="true"
              />
            </ActionButton>
          </div>
        </div>
      ) : null}
    </div>
  );
}

const Turn = memo(function Turn({
  turn,
  version,
  model,
  last,
  busy,
  reduced,
  actions,
}: {
  turn: TurnRecord;
  version: TurnVersion;
  model: ChatModel;
  last: boolean;
  busy: boolean;
  reduced: boolean;
  actions: ChatActions;
}) {
  const ref = useRef<HTMLElement>(null);
  useHeightFlip(ref, version.id, reduced);

  return (
    <article
      ref={ref}
      className={styles.turn}
      data-last={last || undefined}
      aria-label="Message"
    >
      <div key={version.id} className={cx(styles.turnBody, styles.fadeIn)}>
        <UserRow
          turnId={turn.id}
          version={version}
          busy={busy}
          reduced={reduced}
          actions={actions}
        />
        <ReplyView
          turn={turn}
          version={version}
          model={model}
          busy={busy}
          reduced={reduced}
          actions={actions}
        />
      </div>
    </article>
  );
});

/* --------------------------------------------------------------- markdown */

interface InlineSegment {
  text: string;
  start: number;
  strong: boolean;
  code: boolean;
}

interface CiteSegment {
  cite: number;
  start: number;
}

type Segment = InlineSegment | CiteSegment;

interface ListItem {
  start: number;
  segments: Segment[];
}

type Block =
  | {
      kind: "code";
      start: number;
      lang: string;
      code: string;
      codeStart: number;
      closed: boolean;
    }
  | { kind: "h" | "p"; start: number; segments: Segment[] }
  | { kind: "ol" | "ul"; start: number; first: number; items: ListItem[] };

const LIST_LINE = /^(\s*)(\d+\.|[-*])\s+/;
const HEADING_LINE = /^#{1,4}\s+/;
const FENCE_LINE = /^\s*```/;
const CITE = /^\[(\d{1,2})\]/;
const isSpace = (char: string) =>
  char === " " || char === "\n" || char === "\t";

/** Splits one line into runs of plain text, bold, inline code and citation markers. */
function inlineSegments(text: string, offset: number, out: Segment[]) {
  let strong = false;
  let code = false;
  let buffer = "";
  let start = offset;

  const flush = (next: number) => {
    if (buffer) out.push({ text: buffer, start, strong, code });
    buffer = "";
    start = next;
  };

  for (let index = 0; index < text.length;) {
    if (!code && text.startsWith("**", index)) {
      flush(offset + index + 2);
      strong = !strong;
      index += 2;
      continue;
    }
    if (text[index] === "`") {
      flush(offset + index + 1);
      code = !code;
      index += 1;
      continue;
    }
    if (!code && text[index] === "[") {
      const match = CITE.exec(text.slice(index, index + 5));
      if (match) {
        flush(offset + index + match[0].length);
        out.push({ cite: Number(match[1]), start: offset + index });
        index += match[0].length;
        continue;
      }
    }
    if (!buffer) start = offset + index;
    buffer += text[index];
    index += 1;
  }
  flush(offset + text.length);
}

/** Walks to the end of the next word, skipping leading whitespace. */
function nextWord(text: string, from: number) {
  let index = from;
  while (index < text.length && isSpace(text[index])) index += 1;
  while (index < text.length && !isSpace(text[index])) index += 1;
  return index;
}

function parseBlocks(source: string): Block[] {
  const blocks: Block[] = [];
  const lines = source.split("\n");
  const starts: number[] = [];
  lines.reduce((offset, line) => {
    starts.push(offset);
    return offset + line.length + 1;
  }, 0);

  let paragraph: { kind: "p"; start: number; segments: Segment[] } | null =
    null;
  let list: Extract<Block, { kind: "ol" | "ul" }> | null = null;
  let index = 0;

  while (index < lines.length) {
    const line = lines[index];
    const start = starts[index];

    if (FENCE_LINE.test(line)) {
      paragraph = null;
      list = null;
      const lang = line.trim().slice(3).trim();
      const code: string[] = [];
      let cursor = index + 1;
      while (cursor < lines.length && lines[cursor].trim() !== "```")
        code.push(lines[cursor++]);
      blocks.push({
        kind: "code",
        start,
        lang,
        code: code.join("\n"),
        codeStart: starts[index + 1] ?? source.length,
        closed: cursor < lines.length,
      });
      index = cursor + 1;
      continue;
    }

    if (!line.trim()) {
      paragraph = null;
      list = null;
      index += 1;
      continue;
    }

    const listMatch = LIST_LINE.exec(line);
    if (listMatch) {
      paragraph = null;
      const kind = listMatch[2].endsWith(".") ? "ol" : "ul";
      if (!list || list.kind !== kind) {
        list = {
          kind,
          start,
          first: kind === "ol" ? parseInt(listMatch[2], 10) : 1,
          items: [],
        };
        blocks.push(list);
      }
      const segments: Segment[] = [];
      inlineSegments(
        line.slice(listMatch[0].length),
        start + listMatch[0].length,
        segments,
      );
      list.items.push({ start, segments });
      index += 1;
      continue;
    }

    const headingMatch = HEADING_LINE.exec(line);
    if (headingMatch) {
      list = null;
      const block: Block = { kind: "h", start, segments: [] };
      inlineSegments(
        line.slice(headingMatch[0].length),
        start + headingMatch[0].length,
        block.segments,
      );
      blocks.push(block);
      paragraph = null;
      index += 1;
      continue;
    }

    const target = list
      ? list.items[list.items.length - 1].segments
      : paragraph?.segments;
    if (target) {
      target.push({ text: " ", start: start - 1, strong: false, code: false });
      inlineSegments(
        line.trim(),
        start + (line.length - line.trimStart().length),
        target,
      );
    } else {
      paragraph = { kind: "p", start, segments: [] };
      inlineSegments(line, start, paragraph.segments);
      blocks.push(paragraph);
    }
    index += 1;
  }

  return blocks;
}

/** Wraps the text revealed since the last tick in `span.chunk`, so only new text fades in. */
function chunkWrap(
  text: string,
  start: number,
  marks: number[] | undefined,
  render?: (value: string) => ReactNode,
) {
  const paint = render ?? ((value: string) => value);
  if (!marks) return paint(text);
  const end = start + text.length;
  const out: ReactNode[] = [];
  let from = start;
  for (const mark of marks) {
    if (mark <= from) continue;
    if (mark >= end) break;
    out.push(
      <span key={from} className={styles.chunk}>
        {paint(text.slice(from - start, mark - start))}
      </span>,
    );
    from = mark;
  }
  out.push(
    <span key={from} className={styles.chunk}>
      {paint(text.slice(from - start))}
    </span>,
  );
  return out;
}

const SQL_TOKEN =
  /\b(SELECT|FROM|WHERE|AND|OR|NOT|NULL|IS|ALTER|TABLE|ADD|COLUMN|CONSTRAINT|CHECK|VALID|VALIDATE|SET|DROP|UPDATE|INSERT|INTO|VALUES|CREATE|INDEX|CONCURRENTLY|ON|AS|IN|BEGIN|COMMIT|text|bigint|integer|count|now)\b|('[^']*')|(\$\d+|\b\d[\d_]*\b)|(--.*$)/gm;

function tokenize(code: string) {
  const out: ReactNode[] = [];
  let cursor = 0;
  for (const match of code.matchAll(SQL_TOKEN)) {
    const at = match.index ?? 0;
    if (at > cursor) out.push(code.slice(cursor, at));
    const className = match[1]
      ? styles.tokKeyword
      : match[2]
        ? styles.tokString
        : match[3]
          ? styles.tokNumber
          : styles.tokComment;
    out.push(
      <span key={at} className={className}>
        {match[0]}
      </span>,
    );
    cursor = at + match[0].length;
  }
  if (cursor < code.length) out.push(code.slice(cursor));
  return out.length === 1 ? out[0] : out;
}

function SourcePreview({ source, n }: { source: ChatSource; n: number }) {
  const Mark = source.mark;
  return (
    <div className={styles.sourcePreview}>
      <p className={styles.sourceSite}>
        {Mark ? (
          <Mark
            className={styles.sourceMark}
            style={{ color: source.color }}
            aria-hidden="true"
          />
        ) : (
          <Globe
            className={styles.sourceMark}
            size={14}
            strokeWidth={1.75}
            aria-hidden="true"
          />
        )}
        <span>{hostOf(source.url)}</span>
        <span className={styles.sourceN}>{n}</span>
      </p>
      <p className={styles.sourceTitle}>{source.title}</p>
      {source.snippet ? (
        <p className={styles.sourceSnippet}>{source.snippet}</p>
      ) : null}
    </div>
  );
}

function Cite({
  n,
  source,
  fresh,
}: {
  n: number;
  source?: ChatSource;
  fresh: boolean;
}) {
  const className = cx(styles.cite, fresh && styles.chunk);

  if (!source) {
    return (
      <span className={className} data-missing="">
        {n}
      </span>
    );
  }

  return (
    <HoverCard
      side="top"
      align="center"
      openDelay={220}
      className={styles.sourceCard}
      content={<SourcePreview source={source} n={n} />}
    >
      <a
        className={className}
        href={source.url}
        target="_blank"
        rel="noreferrer"
        aria-label={`Source ${n}: ${source.title}, ${hostOf(source.url)}`}
      >
        {n}
      </a>
    </HoverCard>
  );
}

function Inline({
  segments,
  marks,
  sources,
}: {
  segments: Segment[];
  marks?: number[];
  sources: ChatSource[];
}) {
  return (
    <>
      {segments.map((segment) => {
        if ("cite" in segment) {
          return (
            <Cite
              key={`c${segment.start}`}
              n={segment.cite}
              source={sources[segment.cite - 1]}
              fresh={Boolean(marks)}
            />
          );
        }
        const wrapped = chunkWrap(segment.text, segment.start, marks);
        if (segment.code) {
          return (
            <code key={segment.start} className={styles.inlineCode}>
              {wrapped}
            </code>
          );
        }
        if (segment.strong) {
          return (
            <strong key={segment.start} className={styles.strong}>
              {wrapped}
            </strong>
          );
        }
        return <span key={segment.start}>{wrapped}</span>;
      })}
    </>
  );
}

function CodeBlock({
  block,
  marks,
}: {
  block: Extract<Block, { kind: "code" }>;
  marks?: number[];
}) {
  return (
    <div className={cx(styles.codeBlock, Boolean(marks) && styles.chunk)}>
      <div className={styles.codeHead}>
        <span className={styles.codeLang}>{block.lang || "code"}</span>
        <CopyButton
          value={block.code}
          label="Copy"
          variant="plain"
          className={styles.codeCopy}
          disabled={!block.closed}
        />
      </div>
      <pre className={styles.codePre}>
        <code>{chunkWrap(block.code, block.codeStart, marks, tokenize)}</code>
      </pre>
    </div>
  );
}

const Markdown = memo(function Markdown({
  source,
  shown,
  marks,
  sources = [],
}: {
  source: string;
  shown?: number;
  marks?: number[];
  sources?: ChatSource[];
}) {
  const blocks = useMemo(
    () => parseBlocks(shown === undefined ? source : source.slice(0, shown)),
    [source, shown],
  );

  return (
    <div className={styles.markdown}>
      {blocks.map((block) => {
        if (block.kind === "code")
          return (
            <CodeBlock key={`code${block.start}`} block={block} marks={marks} />
          );
        if (!("items" in block)) {
          const content = (
            <Inline segments={block.segments} marks={marks} sources={sources} />
          );
          return block.kind === "h" ? (
            <h3 key={`h${block.start}`} className={styles.heading}>
              {content}
            </h3>
          ) : (
            <p key={`p${block.start}`} className={styles.paragraph}>
              {content}
            </p>
          );
        }
        const items = block.items.map((item) => (
          <li key={item.start} className={marks ? styles.chunk : undefined}>
            <Inline segments={item.segments} marks={marks} sources={sources} />
          </li>
        ));
        return block.kind === "ol" ? (
          <ol
            key={`ol${block.start}`}
            className={styles.list}
            start={block.first !== 1 ? block.first : undefined}
          >
            {items}
          </ol>
        ) : (
          <ul key={`ul${block.start}`} className={styles.list}>
            {items}
          </ul>
        );
      })}
    </div>
  );
});

/* ------------------------------------------------------------- sample data */

/** Models the sample header menu offers, with the mark each one is drawn with. */
export const sampleModels: ChatModel[] = [
  {
    id: "claude-opus-5-5",
    name: "Claude Opus 5.5",
    note: "Anthropic",
    mark: ClaudeMark,
    color: "#D97757",
  },
  {
    id: "claude-sonnet-5-5",
    name: "Claude Sonnet 5.5",
    note: "Anthropic",
    mark: ClaudeMark,
    color: "#D97757",
  },
  { id: "gpt-6-1-sol", name: "GPT-6.1 Sol", note: "OpenAI", mark: OpenAIMark },
  {
    id: "gemini-4-argon",
    name: "Gemini 4 Argon",
    note: "Google",
    mark: GeminiMark,
  },
  {
    id: "mistral-large-4",
    name: "Mistral Large 4",
    note: "Mistral AI",
    mark: MistralMark,
  },
];

const postgres = { mark: PostgresMark, color: "#4169E1" };

const sourceAlterTable: ChatSource = {
  ...postgres,
  title: "PostgreSQL 17: ALTER TABLE",
  url: "https://www.postgresql.org/docs/17/sql-altertable.html",
  snippet:
    "Validating a constraint takes only a SHARE UPDATE EXCLUSIVE lock, and SET NOT NULL skips the table scan when a valid CHECK constraint already proves no NULL can exist.",
};

const sourceLocking: ChatSource = {
  ...postgres,
  title: "PostgreSQL 17: Explicit locking",
  url: "https://www.postgresql.org/docs/17/explicit-locking.html",
  snippet:
    "SHARE UPDATE EXCLUSIVE protects a table against concurrent schema changes and VACUUM runs. It does not conflict with the ROW EXCLUSIVE lock that writes take.",
};

const sourceClientConfig: ChatSource = {
  ...postgres,
  title: "PostgreSQL 17: Client connection defaults",
  url: "https://www.postgresql.org/docs/17/runtime-config-client.html",
  snippet:
    "lock_timeout aborts any statement that waits longer than the given time while trying to acquire a lock.",
};

const sourceCreateIndex: ChatSource = {
  ...postgres,
  title: "PostgreSQL 17: CREATE INDEX",
  url: "https://www.postgresql.org/docs/17/sql-createindex.html",
  snippet:
    "With CONCURRENTLY, PostgreSQL builds the index without taking any locks that prevent concurrent inserts, updates or deletes on the table.",
};

const sourceStrongMigrations: ChatSource = {
  mark: GitHubMark,
  title: "strong_migrations: Setting NOT NULL on an existing column",
  url: "https://github.com/ankane/strong_migrations",
  snippet:
    "Add a check constraint without validating existing rows, validate it in a separate step, then set NOT NULL.",
};

interface SampleScenario {
  id: string;
  match: RegExp;
  title: string;
  reasoning: string;
  seconds: number;
  tool?: ChatTool;
  sources?: ChatSource[];
  text: string;
  /** First line swapped in when the same prompt is retried. */
  retryLead?: string;
  /** Message the first run fails with, so Retry has something to recover from. */
  failFirst?: string;
}

const sampleEstimate: SampleScenario = {
  id: "estimate",
  match: /how long|estimate|backfill take|per batch|duration/i,
  title: "Backfill estimate",
  reasoning:
    "Use the real row count rather than the planner estimate, then multiply the batch count by a realistic time per batch, including a pause for the replicas.",
  seconds: 2,
  tool: {
    id: "count",
    name: "Queried orders",
    activeName: "Querying orders",
    status: "done",
    summary: "40,213,880 rows",
    icon: "database",
    ...postgres,
    input: ["SELECT count(*) FROM orders;"],
    output: "40,213,880",
  },
  text: `About 23 minutes. The table has 40,213,880 rows, so 5,000 rows per batch makes 8,043 batches.

- **Per batch:** about 120 ms for the update and a 50 ms pause so replicas keep up
- **Total:** 8,043 \xd7 170 ms ≈ 1,367 s, or just under 23 minutes
- **Lag:** if replication lag passes 5 s, double the pause instead of shrinking the batch

Walk the primary key in ranges, so every batch uses the index and never rescans finished rows:

\`\`\`sql
UPDATE orders o
SET region = c.region
FROM customers c
WHERE c.id = o.customer_id
  AND o.id >= $1 AND o.id < $1 + 5000
  AND o.region IS NULL;
\`\`\``,
  retryLead:
    "Just under 23 minutes, at about 170 ms for each of the 8,043 batches.",
};

const sampleLag: SampleScenario = {
  id: "lag",
  match: /\blag\b|replica/i,
  title: "Replication lag",
  reasoning:
    "Read the replay delay on the replica itself, then compare it with the threshold the backfill should pause at.",
  seconds: 2,
  tool: {
    id: "lag",
    name: "Queried replica-eu-2",
    activeName: "Querying replica-eu-2",
    status: "done",
    summary: "0.4 s behind",
    icon: "database",
    ...postgres,
    input: ["SELECT now() - pg_last_xact_replay_timestamp() AS lag;"],
    output: "00:00:00.412",
  },
  failFirst:
    "Could not reach replica-eu-2. The connection timed out after 10 seconds.",
  text: `replica-eu-2 is **0.4 seconds** behind the primary, well inside a safe range for a batched backfill.

- **Replay lag:** 0.4 s
- **Received but not replayed:** 2.1 MB of WAL
- **Rule of thumb:** pause the backfill when lag passes 5 s, and resume once it drops under 1 s`,
};

const sampleVerify: SampleScenario = {
  id: "verify",
  match: /$^/,
  title: "",
  reasoning:
    "This preview only knows the orders migration, so answer with the step that usually comes next: checking the backfill before release.",
  seconds: 1,
  text: "In this preview I only know the `orders` migration, so here is the step that usually comes next: check the backfill before you ship.\n\n```sql\nSELECT count(*) FROM orders WHERE region IS NULL;\n\nSELECT conname, convalidated FROM pg_constraint\nWHERE conrelid = 'orders'::regclass;\n```\n\nYou want zero rows without a region, and `convalidated` set to true for the check.",
};

const sampleScenarios: SampleScenario[] = [
  sampleLag,
  {
    id: "rollback",
    match: /roll ?back|undo|revert/i,
    title: "Rollback plan",
    reasoning:
      "Each step is reversible on its own, and the column can stay until the app stops reading it.",
    seconds: 2,
    text: "Each step can be undone on its own, and none of them loses data.\n\n1. **Before `SET NOT NULL`:** drop the check constraint. Writes never noticed it.\n2. **After `SET NOT NULL`:** run `ALTER COLUMN region DROP NOT NULL`, which is instant.\n3. **The column itself:** leave it in place until the app stops reading it, then drop it in a later release.\n\nKeep `region IS NULL` in the backfill's `WHERE` clause, so a rerun after a rollback only touches the rows it missed.",
    retryLead:
      "Every step is reversible, and you can undo them in reverse order without losing data.",
  },
  {
    id: "index",
    match: /index/i,
    title: "Index on region",
    reasoning:
      "A plain CREATE INDEX blocks writes for the whole build. CONCURRENTLY avoids that at the cost of a slower build.",
    seconds: 3,
    tool: {
      id: "search-index",
      name: "Searched the web",
      activeName: "Searching the web",
      status: "done",
      summary: "1 source",
      icon: "search",
      input: ["postgres 17 create index concurrently"],
    },
    sources: [sourceCreateIndex],
    text: "Build it with `CREATE INDEX CONCURRENTLY`, so writes keep flowing while it builds. [1]\n\n```sql\nCREATE INDEX CONCURRENTLY orders_region_idx ON orders (region);\n```\n\nIt takes longer than a normal build and cannot run inside a transaction block. If it fails, it leaves an invalid index behind: drop it and run the build again.",
  },
  sampleEstimate,
  {
    id: "add-column",
    match:
      /not null|non-null|nonnull|add (a |the )?(\w+ )?column|without (blocking|downtime|locking)/i,
    title: "Add region to orders",
    reasoning:
      "A NOT NULL column with no default fails on existing rows, and setting it on a filled column scans 40M rows under an exclusive lock. Safer path: nullable column, batched backfill, a NOT VALID check, validation under a weaker lock, then SET NOT NULL, which can skip the scan. Confirm the lock levels in the docs.",
    seconds: 4,
    tool: {
      id: "search",
      name: "Searched the web",
      activeName: "Searching the web",
      status: "done",
      summary: "4 sources",
      icon: "search",
      input: [
        "postgres 17 set not null without table scan",
        "validate constraint lock level",
      ],
    },
    sources: [
      sourceAlterTable,
      sourceLocking,
      sourceStrongMigrations,
      sourceClientConfig,
    ],
    text: "Split it into four short steps, so no single statement holds a heavy lock for long.\n\n1. **Add the column as nullable.** With no default, this only touches the catalog and returns at once.\n2. **Backfill in batches** of about 5,000 rows, committing each batch so transactions stay short and autovacuum keeps up.\n3. **Add a `NOT VALID` check, then validate it.** Validation scans the table under a `SHARE UPDATE EXCLUSIVE` lock, so reads and writes keep flowing. [1][2]\n4. **Set `NOT NULL`.** Postgres sees the validated check and skips the full scan, so this lock lasts milliseconds. [1][3]\n\n```sql\nALTER TABLE orders ADD COLUMN region text;\n\nALTER TABLE orders ADD CONSTRAINT orders_region_not_null\n  CHECK (region IS NOT NULL) NOT VALID;\n\nALTER TABLE orders VALIDATE CONSTRAINT orders_region_not_null;\nALTER TABLE orders ALTER COLUMN region SET NOT NULL;\nALTER TABLE orders DROP CONSTRAINT orders_region_not_null;\n```\n\nRun each `ALTER` with a short `lock_timeout`, such as `2s`, so a statement stuck behind a long query gives up instead of queueing every write behind it. [4]",
    retryLead:
      "Do it in four steps, each holding a heavy lock for only a moment.",
  },
];

const pickScenario = (prompt: string) =>
  sampleScenarios.find((scenario) => scenario.match.test(prompt)) ??
  sampleVerify;

/** Titles the conversation from the question: the matched scenario name, else its first six words. */
function deriveTitle(prompt: string) {
  const titled = pickScenario(prompt).title;
  if (titled) return titled;
  const trimmed = prompt
    .replace(/[`*?.!]+/g, "")
    .trim()
    .split(/\s+/)
    .slice(0, 6)
    .join(" ");
  return trimmed.length > 40
    ? `${trimmed.slice(0, 38)}…`
    : trimmed || "New chat";
}

/** Prompts the empty state offers before the first message. */
export const sampleSuggestions: ChatSuggestion[] = [
  {
    label: "Add a NOT NULL column without downtime",
    prompt:
      "How do I add a NOT NULL column to a 40M row table without blocking writes?",
    icon: "database",
  },
  {
    label: "Estimate a backfill for 40M rows",
    prompt:
      "How long will a backfill of 40M rows take at 5,000 rows per batch?",
    icon: "timer",
  },
  { label: "Check replication lag on replica-eu-2", icon: "activity" },
];

/** Conversations the sample title menu lists. */
export const sampleHistory: AiChatHistoryItem[] = [
  {
    id: "partition-events",
    title: "Partition the events table",
    group: "Today",
    messages: [
      {
        role: "user",
        content:
          "Should we partition `events` by month or by week? It grows by about 90M rows a month.",
      },
      {
        role: "assistant",
        model: "claude-opus-5-5",
        thoughtFor: 3,
        reasoning:
          "At 90M rows a month the main costs are vacuum time and retention deletes. Monthly partitions keep both cheap without a large partition count.",
        content:
          "By month. At about 90M rows a month, each partition stays small enough for quick vacuums and index builds, and the partition count stays low.\n\n- **Retention:** dropping a month is one `DETACH PARTITION` and a `DROP TABLE`, with no bulk delete\n- **Queries:** filters on `created_at` prune to one or two partitions\n- **Weekly** partitions only pay off once a single month gets slow to vacuum",
      },
    ],
  },
  {
    id: "status-page",
    title: "Status page incident draft",
    group: "Previous 7 days",
    messages: [
      {
        role: "user",
        content:
          "Draft a status page update for the checkout slowdown on October 14. It was resolved at 15:40 UTC.",
      },
      {
        role: "assistant",
        model: "claude-sonnet-5-5",
        thoughtFor: 2,
        reasoning:
          "Lead with the impact and the time window, say plainly that no payment was lost, then the cause and the fix.",
        content: `**Resolved: slower checkout**

Between 14:55 and 15:40 UTC on October 14, some customers saw checkout take longer than usual. No payment was lost or charged twice. A database migration held a lock for longer than expected; we stopped it, and checkout recovered at 15:40 UTC.

We are adding lock timeouts to every migration, so a schema change cannot hold up checkout again.`,
      },
    ],
  },
  {
    id: "churn-themes",
    title: "Churn interview themes",
    group: "Previous 7 days",
    messages: [
      {
        role: "user",
        content:
          "Summarize the themes from the 12 churn interviews in three bullets.",
      },
      {
        role: "assistant",
        model: "gpt-6-1-sol",
        thoughtFor: 5,
        reasoning:
          "Group the notes by the reason each customer gave first, then count how many interviews mention each.",
        content: `- **Setup took too long:** 7 of 12 never finished connecting their data in the first week
- **Pricing jumped at 10 seats:** 4 left at renewal when the team plan started
- **Exports were missing:** 3 needed CSV exports for finance and moved to a tool that had them`,
      },
    ],
  },
];

const SUGGESTION_ICONS = {
  database: Database,
  timer: Timer,
  activity: Activity,
  search: Search,
} as const;

/* ----------------------------------------------------------- thread helpers */

const EMPTY_THREAD: Thread = { root: null, turns: {} };

function newReply(id: string, model: string): ThreadReply {
  return {
    id,
    model,
    status: "thinking",
    text: "",
    shown: 0,
    streamDone: false,
    reasoning: "",
    tools: [],
    sources: [],
    feedback: null,
  };
}

/** Walks the linked list of turns and returns the active version of each, in order. */
function visibleTurns(thread: Thread) {
  const out: Array<{ turn: TurnRecord; version: TurnVersion }> = [];
  const seen = new Set<string>();
  let cursor = thread.root;
  while (cursor && thread.turns[cursor] && !seen.has(cursor)) {
    seen.add(cursor);
    const turn = thread.turns[cursor];
    const version = turn.versions[turn.active];
    out.push({ turn, version });
    cursor = version.next;
  }
  return out;
}

function replyToMessage(reply: ThreadReply): AiChatMessage {
  return {
    id: reply.id,
    role: "assistant",
    content: reply.text.slice(0, reply.shown),
    model: reply.model,
    reasoning: reply.reasoning || undefined,
    thoughtFor: reply.thoughtFor,
    tools: reply.tools,
    sources: reply.sources,
    status:
      reply.status === "stopped"
        ? "stopped"
        : reply.status === "error"
          ? "error"
          : "done",
    error: reply.error,
  };
}

/** Turns a plain message list into the linked turn list the component walks. */
function threadFromMessages(
  messages: AiChatMessage[],
  model: string,
  prefix: string,
): Thread {
  const turns: Record<string, TurnRecord> = {};
  let root: string | null = null;
  let last: TurnVersion | null = null;
  let index = 0;

  for (const message of messages) {
    if (message.role === "user") {
      const turnId = `${prefix}-t${index}`;
      const version: TurnVersion = {
        id: `${prefix}-v${index}`,
        prompt: message.content,
        attachments: message.attachments,
        reply: newReply(`${prefix}-r${index}`, model),
        next: null,
      };
      turns[turnId] = { id: turnId, versions: [version], active: 0 };
      if (last) last.next = turnId;
      else root = turnId;
      last = version;
      index += 1;
    } else if (last) {
      last.reply = {
        ...last.reply,
        id: message.id ?? last.reply.id,
        model: message.model ?? model,
        status: message.status ?? "done",
        text: message.content,
        shown: message.content.length,
        streamDone: true,
        reasoning: message.reasoning ?? "",
        thoughtFor: message.thoughtFor,
        tools: message.tools ?? [],
        sources: message.sources ?? [],
        error: message.error,
      };
    }
  }

  return { root, turns };
}

function withTurn(
  thread: Thread,
  turnId: string,
  update: (turn: TurnRecord) => TurnRecord,
): Thread {
  const turn = thread.turns[turnId];
  return turn
    ? { ...thread, turns: { ...thread.turns, [turnId]: update(turn) } }
    : thread;
}

function withReply(
  thread: Thread,
  turnId: string,
  versionId: string,
  update: (reply: ThreadReply) => ThreadReply,
): Thread {
  return withTurn(thread, turnId, (turn) => ({
    ...turn,
    versions: turn.versions.map((version) =>
      version.id === versionId
        ? { ...version, reply: update(version.reply) }
        : version,
    ),
  }));
}

/** Appends a turn after the last visible one, so the chain stays linear. */
function appendTurn(thread: Thread, turn: TurnRecord): Thread {
  const turns = { ...thread.turns, [turn.id]: turn };
  const visible = visibleTurns(thread);
  const last = visible[visible.length - 1];
  if (!last) return { root: turn.id, turns };
  const previous = turns[last.turn.id];
  turns[previous.id] = {
    ...previous,
    versions: previous.versions.map((version, index) =>
      index === previous.active ? { ...version, next: turn.id } : version,
    ),
  };
  return { root: thread.root, turns };
}

/* -------------------------------------------------------- sample responder */

/** Scripted responder the preview streams when no `respond` prop is given. */
export function createSampleResponder(): ChatRespond {
  const runs = new Map<string, number>();

  return async function* respond(messages, { signal }) {
    const prompt =
      [...messages].reverse().find((message) => message.role === "user")
        ?.content ?? "";
    const scenario = pickScenario(prompt);
    const run = runs.get(scenario.id) ?? 0;
    runs.set(scenario.id, run + 1);

    for (const chunk of chunkText(scenario.reasoning, 34)) {
      await sleep(110, signal);
      if (signal.aborted) return;
      yield { type: "reasoning", delta: chunk };
    }

    yield { type: "reasoning-end", seconds: scenario.seconds };

    if (scenario.tool) {
      yield {
        type: "tool",
        tool: {
          ...scenario.tool,
          status: "running",
          summary: undefined,
          output: undefined,
        },
      };
      await sleep(scenario.failFirst && run === 0 ? 1400 : 850, signal);
      if (signal.aborted) return;

      if (scenario.failFirst && run === 0) {
        yield {
          type: "tool",
          tool: {
            ...scenario.tool,
            status: "error",
            name: scenario.tool.activeName ?? scenario.tool.name,
            summary: "Timed out",
            output: undefined,
          },
        };
        yield { type: "error", message: scenario.failFirst };
        return;
      }

      yield {
        type: "tool",
        tool: { ...scenario.tool, sources: scenario.sources },
      };
    }

    if (scenario.sources) yield { type: "sources", sources: scenario.sources };

    const text =
      run > 0 && scenario.retryLead
        ? scenario.text.replace(/^[^\n]+/, scenario.retryLead)
        : scenario.text;

    await sleep(120, signal);
    for (const chunk of chunkText(text, 42)) {
      if (signal.aborted) return;
      yield { type: "text", delta: chunk };
      await sleep(18, signal);
    }
  };
}

/* ------------------------------------------------------------------- chat */

const DEMO_CHAT = "chat-demo";
let uid = 0;
const nextId = (prefix: string) =>
  `${prefix}-${Date.now().toString(36)}${(uid++).toString(36)}`;

const DEMO_PROMPT =
  "We need a non-null `region` column on our 40M row `orders` table in Postgres 17. How do we add it without blocking writes?";

/** Where the reveal stops next: whole words, never inside a code fence or a bare list marker. */
function revealNext(
  text: string,
  shown: number,
  words: number,
  streamDone: boolean,
) {
  let next = shown;
  for (let step = 0; step < words && next < text.length; step += 1)
    next = nextWord(text, next);

  if (next >= text.length && !streamDone) {
    let end = text.length;
    while (end > shown && !isSpace(text[end - 1])) end -= 1;
    next = end;
  }
  if (next <= shown) return shown;

  for (let attempt = 0; attempt < 4; attempt += 1) {
    const lineStart = text.lastIndexOf("\n", next - 1) + 1;
    const lineEnd = text.indexOf("\n", lineStart);
    const stop = lineEnd === -1 ? text.length : lineEnd;

    if (FENCE_LINE.test(text.slice(lineStart, stop))) {
      if (lineEnd !== -1) return Math.max(next, lineEnd + 1);
      return streamDone ? text.length : Math.max(shown, lineStart);
    }

    if (/^\s*(\d+\.|[-*]|#{1,4})\s*$/.test(text.slice(lineStart, next))) {
      const step = nextWord(text, next);
      if (step >= text.length && !streamDone) return Math.max(shown, lineStart);
      next = step;
      continue;
    }

    break;
  }

  return next;
}

/** The chat itself: header, thread, composer and the status region screen readers follow. */
export function AiChat({
  models = sampleModels,
  respond,
  suggestions = sampleSuggestions,
  greeting = "What are we working on?",
  placeholder = "Ask anything",
  initialMessages,
  title: titleProp,
  history,
  defaultModel,
  autoPlay,
  onFeedback,
  onModelChange,
  onNewChat,
  className,
}: AiChatProps) {
  const reduced = useReduced();
  const playing = autoPlay ?? (!respond && !initialMessages?.length);
  const simulated = !respond;
  const responder = useMemo(
    () => respond ?? createSampleResponder(),
    [respond],
  );

  const [modelId, setModelId] = useState(
    () => defaultModel ?? models[0]?.id ?? "",
  );
  const model = models.find((item) => item.id === modelId) ?? models[0];
  const resolveModel = useCallback(
    (id: string) =>
      models.find((item) => item.id === id) ??
      sampleModels.find((item) => item.id === id) ??
      models[0],
    [models],
  );

  const [chats, setChats] = useState<Chat[]>(() => {
    const earlier = (
      history ?? (respond || initialMessages ? [] : sampleHistory)
    ).map((item) => ({
      id: item.id,
      title: item.title,
      group: item.group ?? "Earlier",
      thread: threadFromMessages(
        item.messages,
        defaultModel ?? models[0]?.id ?? "",
        item.id,
      ),
    }));

    const fallbackModel = defaultModel ?? models[0]?.id ?? "";
    let current: Chat;
    if (initialMessages?.length) {
      const firstUser =
        initialMessages.find((message) => message.role === "user")?.content ??
        "";
      current = {
        id: "chat-current",
        title: titleProp ?? deriveTitle(firstUser),
        group: "Today",
        thread: threadFromMessages(initialMessages, fallbackModel, "initial"),
      };
    } else if (playing) {
      current = {
        id: DEMO_CHAT,
        title: titleProp ?? deriveTitle(DEMO_PROMPT),
        group: "Today",
        thread: threadFromMessages(
          [{ role: "user", content: DEMO_PROMPT }],
          fallbackModel,
          "demo",
        ),
      };
    } else {
      current = {
        id: "chat-new",
        title: titleProp ?? "",
        group: "Today",
        thread: EMPTY_THREAD,
      };
    }

    return [current, ...earlier];
  });

  const [activeId, setActiveId] = useState(() => chats[0].id);
  const chatsRef = useRef(chats);
  const activeRef = useRef(activeId);
  const [input, setInput] = useState("");
  const [attachments, setAttachments] = useState<ChatAttachment[]>([]);
  const [stream, setStream] = useState<ActiveStream | null>(null);
  // The stream identity has to survive a timer closure, so the loop reads the ref, not the render.
  const streamRef = useRef<ActiveStream | null>(null);
  const [live, setLive] = useState({ text: "", n: 0 });
  const [stuck, setStuck] = useState(false);

  const rootRef = useRef<HTMLElement>(null);
  const scrollerRef = useRef<HTMLDivElement>(null);
  const contentRef = useRef<HTMLDivElement>(null);
  const mirrorRef = useRef<HTMLTextAreaElement>(null);
  const inputRef = useRef<HTMLTextAreaElement>(null);
  const pinned = useRef(true);
  const scrollTo = useRef<() => void>(() => {});
  const scrollMemory = useRef(
    new Map<string, { top: number; bottom: boolean }>(),
  );
  const autoplayStep = useRef(playing ? 0 : -1);
  const autoplayTimer = useRef(0);
  const reducedRef = useRef(reduced);
  const inView = useInView(rootRef, { once: true, amount: 0.2 });

  const chat = chats.find((item) => item.id === activeId) ?? chats[0];
  const turns = visibleTurns(chat.thread);
  const busy = stream?.chatId === chat.id;

  const setChatList = useCallback((update: (current: Chat[]) => Chat[]) => {
    chatsRef.current = update(chatsRef.current);
    setChats(chatsRef.current);
  }, []);

  const updateThread = useCallback(
    (chatId: string, update: (thread: Thread) => Thread) =>
      setChatList((current) =>
        current.map((item) =>
          item.id === chatId ? { ...item, thread: update(item.thread) } : item,
        ),
      ),
    [setChatList],
  );

  const findReply = (active: ActiveStream) =>
    chatsRef.current
      .find((item) => item.id === active.chatId)
      ?.thread.turns[active.turnId]?.versions.find(
        (version) => version.id === active.versionId,
      )?.reply;

  const announce = useCallback(
    (text: string) => setLive((current) => ({ text, n: current.n + 1 })),
    [],
  );
  const suffix = simulated ? " (simulated)" : "";

  function stopAutoplay() {
    autoplayStep.current = -1;
    window.clearTimeout(autoplayTimer.current);
  }

  function finish(
    active: ActiveStream,
    status: "done" | "error" | "stopped",
    message?: string,
  ) {
    if (streamRef.current !== active) return;
    streamRef.current = null;
    setStream(null);
    announce(
      status === "done"
        ? `Reply finished${suffix}`
        : status === "error"
          ? `Reply failed: ${message}`
          : "Reply stopped",
    );

    window.setTimeout(
      () =>
        updateThread(active.chatId, (thread) =>
          withReply(thread, active.turnId, active.versionId, (reply) =>
            reply.marks ? { ...reply, marks: undefined } : reply,
          ),
        ),
      240,
    );

    if (status !== "done" || active.chatId !== DEMO_CHAT) return;
    if (autoplayStep.current === 0) {
      autoplayStep.current = 1;
      autoplayTimer.current = window.setTimeout(() => {
        if (
          autoplayStep.current !== 1 ||
          activeRef.current !== DEMO_CHAT ||
          streamRef.current ||
          inputRef.current?.value
        )
          return;
        autoplayStep.current = 2;
        sendRef.current.send(
          "How long will the backfill take at 5,000 rows per batch?",
          [],
          true,
        );
      }, 1500);
    } else if (autoplayStep.current === 2) {
      autoplayStep.current = -1;
    }
  }

  async function beginStream(
    chatId: string,
    turnId: string,
    versionId: string,
  ) {
    const active: ActiveStream = {
      controller: new AbortController(),
      chatId,
      turnId,
      versionId,
      started: performance.now(),
    };
    const { signal } = active.controller;
    streamRef.current = active;
    setStream(active);

    const reply = findReply(active);
    const currentModel = resolveModel(reply?.model ?? modelId);
    announce(`${currentModel.name} is replying${suffix}`);

    const patch = (update: (reply: ThreadReply) => ThreadReply) =>
      updateThread(chatId, (thread) =>
        withReply(thread, turnId, versionId, update),
      );
    const seconds = () =>
      Math.max(1, Math.round((performance.now() - active.started) / 1000));

    try {
      const thread =
        chatsRef.current.find((item) => item.id === chatId)?.thread ??
        EMPTY_THREAD;
      const history: AiChatMessage[] = [];
      for (const entry of visibleTurns(thread)) {
        history.push({
          id: entry.version.id,
          role: "user",
          content: entry.version.prompt,
          attachments: entry.version.attachments,
        });
        if (entry.turn.id === turnId) break;
        history.push(replyToMessage(entry.version.reply));
      }

      for await (const event of responder(history, {
        signal,
        model: currentModel,
      })) {
        if (signal.aborted) return;

        if (event.type === "reasoning") {
          patch((current) => ({
            ...current,
            reasoning: current.reasoning + event.delta,
          }));
        } else if (event.type === "reasoning-end") {
          patch((current) => ({
            ...current,
            thoughtFor: event.seconds ?? seconds(),
          }));
        } else if (event.type === "tool") {
          patch((current) => ({
            ...current,
            tools: current.tools.some((tool) => tool.id === event.tool.id)
              ? current.tools.map((tool) =>
                  tool.id === event.tool.id ? { ...tool, ...event.tool } : tool,
                )
              : [...current.tools, event.tool],
          }));
        } else if (event.type === "sources") {
          patch((current) => ({ ...current, sources: event.sources }));
        } else if (event.type === "text") {
          patch((current) => ({
            ...current,
            text: current.text + event.delta,
            status: "streaming",
            thoughtFor:
              current.thoughtFor ?? (current.reasoning ? seconds() : undefined),
          }));
        } else if (event.type === "error") {
          throw new Error(event.message);
        }
      }

      if (signal.aborted) return;
      patch((current) => ({
        ...current,
        streamDone: true,
        status: current.text ? "streaming" : "done",
        thoughtFor:
          current.thoughtFor ?? (current.reasoning ? seconds() : undefined),
      }));
      if (!findReply(active)?.text) finish(active, "done");
    } catch (error) {
      if (signal.aborted) return;
      const message = error instanceof Error ? error.message : String(error);
      patch((current) => ({
        ...current,
        status: "error",
        error: message,
        text: current.text.slice(0, current.shown),
        marks: undefined,
        streamDone: true,
        tools: current.tools.map((tool) =>
          tool.status === "running" ? { ...tool, status: "error" } : tool,
        ),
      }));
      finish(active, "error", message);
    }
  }

  function stopReply() {
    const active = streamRef.current;
    if (!active) return;
    active.controller.abort();
    const seconds = Math.max(
      1,
      Math.round((performance.now() - active.started) / 1000),
    );
    updateThread(active.chatId, (thread) =>
      withReply(thread, active.turnId, active.versionId, (reply) => ({
        ...reply,
        status: "stopped",
        text: reply.text.slice(0, reply.shown),
        streamDone: true,
        thoughtFor: reply.thoughtFor ?? (reply.reasoning ? seconds : undefined),
        tools: reply.tools.map((tool) =>
          tool.status === "running"
            ? {
                ...tool,
                status: "error",
                name: tool.activeName ?? tool.name,
                summary: "Stopped",
              }
            : tool,
        ),
      })),
    );
    finish(active, "stopped");
  }

  function scrollToBottom() {
    pinned.current = true;
    setStuck(false);
    scrollTo.current();
  }

  function send(
    text: string,
    files: ChatAttachment[] = attachments,
    silent = false,
  ) {
    const prompt = text.trim();
    if (!prompt && !files.length) return;
    if (!silent) stopAutoplay();
    if (stream) stopReply();

    const chatId = activeRef.current;
    const turnId = nextId("turn");
    const versionId = nextId("version");

    updateThread(chatId, (thread) =>
      appendTurn(thread, {
        id: turnId,
        versions: [
          {
            id: versionId,
            prompt,
            attachments: files.length ? files : undefined,
            reply: newReply(nextId("reply"), modelId),
            next: null,
          },
        ],
        active: 0,
      }),
    );

    setChatList((current) =>
      current.map((item) =>
        item.id !== chatId || item.title
          ? item
          : { ...item, title: deriveTitle(prompt || files[0].name) },
      ),
    );

    if (!silent) {
      setInput("");
      setAttachments([]);
    }
    scrollToBottom();
    void beginStream(chatId, turnId, versionId);
  }

  function retryVersion(turnId: string, prompt?: string) {
    stopAutoplay();
    if (stream) stopReply();
    const chatId = activeRef.current;
    const turn = chatsRef.current.find((item) => item.id === chatId)?.thread
      .turns[turnId];
    if (!turn) return;
    const current = turn.versions[turn.active];
    const versionId = nextId("version");

    updateThread(chatId, (thread) =>
      withTurn(thread, turnId, (record) => ({
        ...record,
        active: record.versions.length,
        versions: [
          ...record.versions,
          {
            id: versionId,
            prompt: prompt ?? current.prompt,
            attachments: current.attachments,
            reply: newReply(nextId("reply"), modelId),
            next: null,
          },
        ],
      })),
    );

    scrollToBottom();
    void beginStream(chatId, turnId, versionId);
  }

  const actions: ChatActions = {
    retry: (turnId) => retryVersion(turnId),
    edit: (turnId, prompt) => retryVersion(turnId, prompt),
    retryInPlace: (turnId, versionId) => {
      stopAutoplay();
      if (stream) stopReply();
      const chatId = activeRef.current;
      updateThread(chatId, (thread) =>
        withReply(thread, turnId, versionId, () =>
          newReply(nextId("reply"), modelId),
        ),
      );
      scrollToBottom();
      void beginStream(chatId, turnId, versionId);
    },
    select: (turnId, index) => {
      updateThread(activeRef.current, (thread) =>
        withTurn(thread, turnId, (turn) => ({
          ...turn,
          active: Math.max(0, Math.min(turn.versions.length - 1, index)),
        })),
      );
      const turn = chatsRef.current.find(
        (item) => item.id === activeRef.current,
      )?.thread.turns[turnId];
      if (turn)
        announce(`Version ${turn.active + 1} of ${turn.versions.length}`);
    },
    feedback: (turnId, versionId, value) => {
      const reply = findReply({
        chatId: activeRef.current,
        turnId,
        versionId,
      } as ActiveStream);
      if (!reply) return;
      const next = reply.feedback === value ? null : value;
      updateThread(activeRef.current, (thread) =>
        withReply(thread, turnId, versionId, (current) => ({
          ...current,
          feedback: next,
        })),
      );
      onFeedback?.(replyToMessage(reply), next);
      announce(
        next
          ? `Feedback sent: ${next === "up" ? "good" : "bad"} response${onFeedback ? "" : " (simulated)"}`
          : "Feedback removed",
      );
    },
    copied: (ok) => announce(ok ? "Copied to clipboard" : "Could not copy"),
  };
  const actionsRef = useRef(actions);
  useLayoutEffect(() => {
    actionsRef.current = actions;
  });
  const stableActions = useMemo<ChatActions>(
    () => ({
      retry: (...args) => actionsRef.current.retry(...args),
      edit: (...args) => actionsRef.current.edit(...args),
      retryInPlace: (...args) => actionsRef.current.retryInPlace(...args),
      select: (...args) => actionsRef.current.select(...args),
      feedback: (...args) => actionsRef.current.feedback(...args),
      copied: (...args) => actionsRef.current.copied(...args),
    }),
    [],
  );

  const sendRef = useRef({ send });
  useLayoutEffect(() => {
    sendRef.current = { send };
  });

  function saveScroll() {
    const scroller = scrollerRef.current;
    if (scroller)
      scrollMemory.current.set(activeRef.current, {
        top: scroller.scrollTop,
        bottom: pinned.current,
      });
  }

  function openChat(id: string) {
    if (id === activeRef.current) return;
    stopAutoplay();
    saveScroll();
    activeRef.current = id;
    setActiveId(id);
    const next = chatsRef.current.find((item) => item.id === id);
    announce(`Opened ${next?.title || "untitled chat"}`);
  }

  function newChat() {
    stopAutoplay();
    const empty = chatsRef.current.find(
      (item) => !item.thread.root && !item.title,
    );
    if (empty) {
      openChat(empty.id);
    } else {
      const id = nextId("chat");
      saveScroll();
      setChatList((current) => [
        { id, title: "", group: "Today", thread: EMPTY_THREAD },
        ...current,
      ]);
      activeRef.current = id;
      setActiveId(id);
      announce("New chat");
    }
    onNewChat?.();
    requestAnimationFrame(() =>
      inputRef.current?.focus({ preventScroll: true }),
    );
  }

  useEffect(() => {
    reducedRef.current = reduced;
  }, [reduced]);

  // Reveal clock: advances `shown` in whole words so each new piece can fade in on its own.
  useEffect(() => {
    if (!stream) return;
    let frame = 0;
    let last = 0;

    const tick = (now: number) => {
      frame = requestAnimationFrame(tick);
      if (now - last < 34) return;
      last = now;

      const reply = findReply(stream);
      if (!reply || reply.status !== "streaming") return;

      if (reply.shown >= reply.text.length) {
        if (reply.streamDone) {
          updateThread(stream.chatId, (thread) =>
            withReply(thread, stream.turnId, stream.versionId, (current) => ({
              ...current,
              status: "done",
            })),
          );
          finish(stream, "done");
        }
        return;
      }

      const next = revealNext(
        reply.text,
        reply.shown,
        reply.text.length - reply.shown > 600 ? 4 : 3,
        reply.streamDone,
      );

      if (next > reply.shown) {
        updateThread(stream.chatId, (thread) =>
          withReply(thread, stream.turnId, stream.versionId, (current) => ({
            ...current,
            shown: next,
            marks: [...(current.marks ?? []), current.shown],
          })),
        );
      }
    };

    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  }, [stream, updateThread]);

  // Scroll anchoring: stay pinned to the latest message on a soft lerp until the reader scrolls up.
  useEffect(() => {
    const scroller = scrollerRef.current;
    const content = contentRef.current;
    if (!scroller || !content || typeof ResizeObserver === "undefined") return;

    let frame = 0;
    let touchY = 0;

    const run = () => {
      frame = 0;
      if (!pinned.current) return;
      const diff =
        scroller.scrollHeight - scroller.clientHeight - scroller.scrollTop;
      if (diff <= 0.5) return;
      scroller.scrollTop += reducedRef.current
        ? diff
        : Math.max(1, 0.22 * diff);
      frame = requestAnimationFrame(run);
    };

    scrollTo.current = () => {
      if (!frame) frame = requestAnimationFrame(run);
    };

    const observer = new ResizeObserver(() => scrollTo.current());
    observer.observe(content);
    observer.observe(scroller);

    const unpin = () => {
      pinned.current = false;
    };
    const onWheel = (event: WheelEvent) => {
      if (event.deltaY < 0) unpin();
    };
    const onTouchStart = (event: TouchEvent) => {
      touchY = event.touches[0]?.clientY ?? 0;
    };
    const onTouchMove = (event: TouchEvent) => {
      if ((event.touches[0]?.clientY ?? 0) > touchY + 4) unpin();
    };
    const onKeyDown = (event: KeyboardEvent) => {
      if (["ArrowUp", "PageUp", "Home"].includes(event.key)) unpin();
    };
    const onPointerDown = (event: PointerEvent) => {
      if (event.target === scroller) unpin();
    };
    const onScroll = () => {
      const atBottom =
        scroller.scrollHeight - scroller.clientHeight - scroller.scrollTop < 8;
      if (atBottom) pinned.current = true;
      setStuck(!atBottom && !pinned.current);
    };

    scroller.addEventListener("wheel", onWheel, { passive: true });
    scroller.addEventListener("touchstart", onTouchStart, { passive: true });
    scroller.addEventListener("touchmove", onTouchMove, { passive: true });
    scroller.addEventListener("keydown", onKeyDown);
    scroller.addEventListener("pointerdown", onPointerDown);
    scroller.addEventListener("scroll", onScroll, { passive: true });

    return () => {
      cancelAnimationFrame(frame);
      observer.disconnect();
      scroller.removeEventListener("wheel", onWheel);
      scroller.removeEventListener("touchstart", onTouchStart);
      scroller.removeEventListener("touchmove", onTouchMove);
      scroller.removeEventListener("keydown", onKeyDown);
      scroller.removeEventListener("pointerdown", onPointerDown);
      scroller.removeEventListener("scroll", onScroll);
    };
  }, []);

  // The scripted conversation only starts once the card is actually on screen.
  useEffect(() => {
    if (!inView || stream) return;
    const chat = chatsRef.current.find((item) => item.id === activeRef.current);
    const last = chat ? visibleTurns(chat.thread).at(-1) : undefined;
    if (!chat || !last || last.version.reply.status !== "thinking") return;

    const timer = window.setTimeout(
      () => {
        if (!stream) void beginStream(chat.id, last.turn.id, last.version.id);
      },
      450 * (autoplayStep.current === 0 ? 1 : 0),
    );
    return () => window.clearTimeout(timer);
  }, [inView]);

  useEffect(
    () => () => {
      window.clearTimeout(autoplayTimer.current);
      streamRef.current?.controller.abort();
    },
    [],
  );

  const groups = useMemo(() => {
    const order: string[] = [];
    const grouped = new Map<string, Chat[]>();
    for (const item of chats) {
      if (!grouped.has(item.group)) {
        grouped.set(item.group, []);
        order.push(item.group);
      }
      grouped.get(item.group)!.push(item);
    }
    return order.map((name) => ({ name, chats: grouped.get(name)! }));
  }, [chats]);

  const title = chat.title || "New chat";

  return (
    <section
      ref={rootRef}
      className={cx(styles.root, className)}
      aria-label="AI chat"
    >
      <header className={styles.header}>
        <DropdownMenu.Root modal={false}>
          <DropdownMenu.Trigger
            className={styles.titleButton}
            aria-label={`Chats, current: ${title}`}
          >
            <Swap
              swapKey={`${chat.id}:${title}`}
              reduced={reduced}
              className={styles.titleSwap}
            >
              <span className={styles.titleText}>{title}</span>
            </Swap>
            <ChevronDown
              className={styles.chipChevron}
              size={14}
              strokeWidth={1.75}
              aria-hidden="true"
            />
          </DropdownMenu.Trigger>
          <DropdownMenu.Portal>
            <DropdownMenu.Content
              className={cx(styles.menu, styles.chatMenu)}
              align="start"
              sideOffset={6}
              collisionPadding={12}
              loop
            >
              <DropdownMenu.Item className={styles.menuItem} onSelect={newChat}>
                <SquarePen size={15} strokeWidth={1.75} aria-hidden="true" />
                <span className={styles.menuName}>New chat</span>
              </DropdownMenu.Item>
              <DropdownMenu.Separator className={styles.menuSeparator} />
              {groups.map((group) => (
                <DropdownMenu.RadioGroup
                  key={group.name}
                  value={chat.id}
                  onValueChange={openChat}
                >
                  <DropdownMenu.Label className={styles.menuLabel}>
                    {group.name}
                  </DropdownMenu.Label>
                  {group.chats.map((item) => (
                    <DropdownMenu.RadioItem
                      key={item.id}
                      value={item.id}
                      className={styles.menuItem}
                    >
                      <span className={styles.menuName}>
                        {item.title || "Untitled chat"}
                      </span>
                      <span className={styles.menuCheck}>
                        <DropdownMenu.ItemIndicator>
                          <Check
                            size={15}
                            strokeWidth={1.75}
                            aria-hidden="true"
                          />
                        </DropdownMenu.ItemIndicator>
                      </span>
                    </DropdownMenu.RadioItem>
                  ))}
                </DropdownMenu.RadioGroup>
              ))}
            </DropdownMenu.Content>
          </DropdownMenu.Portal>
        </DropdownMenu.Root>

        <span className={styles.spacer} />

        {model ? (
          <ModelPicker
            models={models}
            value={model}
            reduced={reduced}
            onChange={(id) => {
              const next = models.find((item) => item.id === id);
              if (!next || next.id === modelId) return;
              setModelId(next.id);
              onModelChange?.(next);
              announce(`${next.name} selected. New replies use it.`);
            }}
          />
        ) : null}

        <Tooltip content="New chat" side="bottom">
          <button
            type="button"
            className={styles.iconButton}
            aria-label="New chat"
            onClick={newChat}
          >
            <SquarePen size={18} strokeWidth={1.75} aria-hidden="true" />
          </button>
        </Tooltip>
      </header>

      <div className={styles.body}>
        <div ref={scrollerRef} className={styles.scroller}>
          <div ref={contentRef} className={styles.content}>
            <AnimatePresence mode="wait" initial={false}>
              <ChatPane
                key={chat.id}
                id={chat.id}
                reduced={reduced}
                onShow={(id) => {
                  const scroller = scrollerRef.current;
                  if (!scroller) return;
                  const memory = scrollMemory.current.get(id);
                  if (!memory || memory.bottom) {
                    scroller.scrollTop = scroller.scrollHeight;
                    pinned.current = true;
                    setStuck(false);
                  } else {
                    scroller.scrollTop = memory.top;
                    pinned.current = false;
                    setStuck(
                      scroller.scrollHeight -
                        scroller.clientHeight -
                        memory.top >
                        8,
                    );
                  }
                }}
              >
                {turns.length ? (
                  <div className={styles.thread}>
                    {turns.map(({ turn, version }, index) => (
                      <Turn
                        key={turn.id}
                        turn={turn}
                        version={version}
                        model={resolveModel(version.reply.model)}
                        last={index === turns.length - 1}
                        busy={busy}
                        reduced={reduced}
                        actions={stableActions}
                      />
                    ))}
                  </div>
                ) : (
                  <div className={styles.empty}>
                    <h2 className={styles.greeting}>{greeting}</h2>
                    <ul className={styles.suggestions}>
                      {suggestions.map((suggestion) => {
                        const Icon =
                          SUGGESTION_ICONS[suggestion.icon ?? "search"];
                        return (
                          <li key={suggestion.label}>
                            <button
                              type="button"
                              className={styles.suggestion}
                              onClick={() =>
                                send(suggestion.prompt ?? suggestion.label, [])
                              }
                            >
                              <Icon
                                size={16}
                                strokeWidth={1.75}
                                aria-hidden="true"
                              />
                              {suggestion.label}
                            </button>
                          </li>
                        );
                      })}
                    </ul>
                  </div>
                )}
              </ChatPane>
            </AnimatePresence>
          </div>
        </div>

        <AnimatePresence>
          {stuck ? (
            <motion.button
              key="jump"
              type="button"
              className={styles.jump}
              aria-label="Scroll to latest message"
              initial={{ opacity: 0, y: 4 * (reduced ? 0 : 1) }}
              animate={{ opacity: 1, y: 0 }}
              exit={{
                opacity: 0,
                y: 4 * (reduced ? 0 : 1),
                transition: { duration: 0.1 },
              }}
              transition={{ duration: 0.16, ease: [...ENTER] }}
              onClick={() => {
                pinned.current = true;
                setStuck(false);
                scrollerRef.current?.scrollTo({
                  top: scrollerRef.current.scrollHeight,
                  behavior: reduced ? "auto" : "smooth",
                });
              }}
            >
              <ArrowDown size={16} strokeWidth={1.75} aria-hidden="true" />
            </motion.button>
          ) : null}
        </AnimatePresence>
      </div>

      <div className={styles.composerDock}>
        <Composer
          value={input}
          busy={busy}
          placeholder={placeholder}
          attachments={attachments}
          inputRef={inputRef}
          reduced={reduced}
          onChange={(value) => {
            stopAutoplay();
            setInput(value);
          }}
          onSubmit={() => send(input)}
          onStop={stopReply}
          onAttach={(files) => {
            stopAutoplay();
            setAttachments((current) =>
              [
                ...current,
                ...files.map((file) => ({
                  id: nextId("file"),
                  name: file.name,
                  size: file.size,
                  type: file.type,
                })),
              ].slice(0, 4),
            );
            announce(
              `${files.length === 1 ? files[0].name : `${files.length} files`} attached`,
            );
          }}
          onRemove={(id) =>
            setAttachments((current) =>
              current.filter((file) => file.id !== id),
            )
          }
        />
      </div>

      <p className={styles.srOnly} role="status" aria-live="polite">
        {live.text}
        {live.n % 2 ? "\u00a0" : ""}
      </p>
    </section>
  );
}

function ChatPane({
  id,
  reduced,
  onShow,
  children,
}: {
  id: string;
  reduced: boolean;
  onShow: (id: string) => void;
  children: ReactNode;
}) {
  const show = useRef(onShow);

  useLayoutEffect(() => {
    show.current(id);
  }, [id]);

  return (
    <motion.div
      className={styles.pane}
      initial={{ opacity: 0, y: 4 * (reduced ? 0 : 1) }}
      animate={{ opacity: 1, y: 0 }}
      exit={{ opacity: 0, transition: { duration: 0.1 * (reduced ? 0 : 1) } }}
      transition={{ duration: 0.16 * (reduced ? 0 : 1), ease: [...ENTER] }}
    >
      {children}
    </motion.div>
  );
}

export default AiChat;
