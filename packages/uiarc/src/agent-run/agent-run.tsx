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
  type ReactNode,
} from "react";
import { AnimatePresence, motion, useInView, useReducedMotion } from "motion/react";
import { animate } from "motion/react";
import {
  ChevronDown,
  Circle,
  CircleCheck,
  CircleMinus,
  CornerUpLeft,
  FileDiff,
  FilePen,
  FileCodeCorner,
  FileText,
  Hand,
  ListChecks,
  LoaderCircle,
  Play,
  RotateCcw,
  Search,
  Terminal,
  createLucideIcon,
} from "lucide-react";
import Button from "../button/button";
import TextShimmer from "../text-shimmer/text-shimmer";
import Tooltip from "../tooltip/tooltip";
import { Input } from "../input/input";
import { motionTokens } from "../lib/motion-tokens";
import styles from "./agent-run.module.css";

/* -------------------------------------------------------------------------------------------------
 * Brands
 * -----------------------------------------------------------------------------------------------*/

interface MarkProps {
  title?: string;
  className?: string;
  "aria-hidden"?: "true" | boolean;
}

/** The GitHub mark, used on the pull request button and the `github` step icon. */
function GitHubMark({ title = "GitHub", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" />
    </svg>
  );
}

/** The npm mark, used for the `npm` step icon. */
function NpmMark({ title = "npm", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="M1.763 0C.786 0 0 .786 0 1.763v20.474C0 23.214.786 24 1.763 24h20.474c.977 0 1.763-.786 1.763-1.763V1.763C24 .786 23.214 0 22.237 0zM5.13 5.323l13.837.019-.009 13.836h-3.464l.01-10.382h-3.456L12.04 19.17H5.113z" />
    </svg>
  );
}

/** The TypeScript mark, used for the `typescript` step icon. */
function TypeScriptMark({ title = "TypeScript", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="M1.125 0C.502 0 0 .502 0 1.125v21.75C0 23.498.502 24 1.125 24h21.75c.623 0 1.125-.502 1.125-1.125V1.125C24 .502 23.498 0 22.875 0zm17.363 9.75c.612 0 1.154.037 1.627.111a6.38 6.38 0 0 1 1.306.34v2.458a3.95 3.95 0 0 0-.643-.361 5.093 5.093 0 0 0-.717-.26 5.453 5.453 0 0 0-1.426-.2c-.3 0-.573.028-.819.086a2.1 2.1 0 0 0-.623.242c-.17.104-.3.229-.393.374a.888.888 0 0 0-.14.49c0 .196.053.373.156.529.104.156.252.304.443.444s.423.276.696.41c.273.135.582.274.926.416.47.197.892.407 1.266.628.374.222.695.473.963.753.268.279.472.598.614.957.142.359.214.776.214 1.253 0 .657-.125 1.21-.373 1.656a3.033 3.033 0 0 1-1.012 1.085 4.38 4.38 0 0 1-1.487.596c-.566.12-1.163.18-1.79.18a9.916 9.916 0 0 1-1.84-.164 5.544 5.544 0 0 1-1.512-.493v-2.63a5.033 5.033 0 0 0 3.237 1.2c.333 0 .624-.03.872-.09.249-.06.456-.144.623-.25.166-.108.29-.234.373-.38a1.023 1.023 0 0 0-.074-1.089 2.12 2.12 0 0 0-.537-.5 5.597 5.597 0 0 0-.807-.444 27.72 27.72 0 0 0-1.007-.436c-.918-.383-1.602-.852-2.053-1.405-.45-.553-.676-1.222-.676-2.005 0-.614.123-1.141.369-1.582.246-.441.58-.804 1.004-1.089a4.494 4.494 0 0 1 1.47-.629 7.536 7.536 0 0 1 1.77-.201zm-15.113.188h9.563v2.166H9.506v9.646H6.789v-9.646H3.375z" />
    </svg>
  );
}

/** The Claude mark, the default model badge. */
function ClaudeMark({ title = "Claude", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="m4.7144 15.9555 4.7174-2.6471.079-.2307-.079-.1275h-.2307l-.7893-.0486-2.6956-.0729-2.3375-.0971-2.2646-.1214-.5707-.1215-.5343-.7042.0546-.3522.4797-.3218.686.0608 1.5179.1032 2.2767.1578 1.6514.0972 2.4468.255h.3886l.0546-.1579-.1336-.0971-.1032-.0972L6.973 9.8356l-2.55-1.6879-1.3356-.9714-.7225-.4918-.3643-.4614-.1578-1.0078.6557-.7225.8803.0607.2246.0607.8925.686 1.9064 1.4754 2.4893 1.8336.3643.3035.1457-.1032.0182-.0728-.164-.2733-1.3539-2.4467-1.445-2.4893-.6435-1.032-.17-.6194c-.0607-.255-.1032-.4674-.1032-.7285L6.287.1335 6.6997 0l.9957.1336.419.3642.6192 1.4147 1.0018 2.2282 1.5543 3.0296.4553.8985.2429.8318.091.255h.1579v-.1457l.1275-1.706.2368-2.0947.2307-2.6957.0789-.7589.3764-.9107.7468-.4918.5828.2793.4797.686-.0668.4433-.2853 1.8517-.5586 2.9021-.3643 1.9429h.2125l.2429-.2429.9835-1.3053 1.6514-2.0643.7286-.8196.85-.9046.5464-.4311h1.0321l.759 1.1293-.34 1.1657-1.0625 1.3478-.8804 1.1414-1.2628 1.7-.7893 1.36.0729.1093.1882-.0183 2.8535-.607 1.5421-.2794 1.8396-.3157.8318.3886.091.3946-.3278.8075-1.967.4857-2.3072.4614-3.4364.8136-.0425.0304.0486.0607 1.5482.1457.6618.0364h1.621l3.0175.2247.7892.522.4736.6376-.079.4857-1.2142.6193-1.6393-.3886-3.825-.9107-1.3113-.3279h-.1822v.1093l1.0929 1.0686 2.0035 1.8092 2.5075 2.3314.1275.5768-.3218.4554-.34-.0486-2.2039-1.6575-.85-.7468-1.9246-1.621h-.1275v.17l.4432.6496 2.3436 3.5214.1214 1.0807-.17.3521-.6071.2125-.6679-.1214-1.3721-1.9246L14.38 17.959l-1.1414-1.9428-.1397.079-.674 7.2552-.3156.3703-.7286.2793-.6071-.4614-.3218-.7468.3218-1.4753.3886-1.9246.3157-1.53.2853-1.9004.17-.6314-.0121-.0425-.1397.0182-1.4328 1.9672-2.1796 2.9446-1.7243 1.8456-.4128.164-.7164-.3704.0667-.6618.4008-.5889 2.386-3.0357 1.4389-1.882.929-1.0868-.0062-.1579h-.0546l-6.3385 4.1164-1.1293.1457-.4857-.4554.0608-.7467.2307-.2429 1.9064-1.3114Z" />
    </svg>
  );
}

/* -------------------------------------------------------------------------------------------------
 * Types
 * -----------------------------------------------------------------------------------------------*/

/** The icon family shown at the start of a step row. */
export type AgentMark =
  | "think"
  | "read"
  | "search"
  | "edit"
  | "terminal"
  | "review"
  | "npm"
  | "typescript"
  | "github";

/** Where a single step sits in the run. */
export type StepStatus = "running" | "done" | "waiting" | "rejected";

/** The run's overall phase. */
export type RunStatus = "idle" | "working" | "waiting" | "done" | "stopped";

/** A line of a changed file, in unified diff form. */
export interface ChangedLine {
  /** Which side of the diff the line is on. */
  type: "add" | "del" | "ctx";
  /** Line number on the old side, for context and removed lines. */
  old?: number;
  /** Line number on the new side, for context and added lines. */
  new?: number;
  /** Line text without the leading `+`, `-` or space. */
  text: string;
}

/** One file the agent proposes to change. */
export interface ChangedFile {
  /** Repository relative path, also the tab label. */
  path: string;
  /** Whether the file is created or edited in place. */
  status: "added" | "modified";
  /** The file's lines, already diffed. */
  lines: ChangedLine[];
}

/** The change an agent is asking you to approve. */
export interface AgentChange {
  /** Stable id, used to key the approval gate. */
  id: string;
  /** One sentence describing the change. */
  summary: string;
  /** The files the change touches. */
  files: ChangedFile[];
  /** What the run reports once the change lands, used as the result summary. */
  outcome?: string;
}

/** A shell or tool exchange shown under a step. */
export interface StepIo {
  /** The command or tool call, printed as-is. */
  input: string;
  /** The lines the tool printed back. */
  output: string[];
}

/** One step of a scripted run. */
export interface ScriptStep {
  /** Stable id, used for keys and the details region id. */
  id: string;
  /** Icon family for the row. */
  mark: AgentMark;
  /** Present-tense label while the step runs. */
  verb: string;
  /** Past-tense label once the step is done. */
  doneVerb: string;
  /** Object of the verb, such as a path or a search phrase. */
  target?: string;
  /** Object of the verb once the step is done, when it differs. */
  doneTarget?: string;
  /** How long the step takes in the simulation, in milliseconds. */
  ms: number;
  /** Total unit count, so a running step can report "4 of 12". */
  total?: number;
  /** Expandable tool details. */
  io?: StepIo;
  /** Expandable reasoning text. */
  text?: string;
  /** Fully custom icon, overriding `mark`. */
  icon?: ReactNode;
  /** A change to approve before the run continues. */
  change?: AgentChange;
  /** Replacement steps after a rejection, plus the follow-up approval. */
  revise?: { steps: ScriptStep[]; approval: ScriptStep };
}

/** What the run reports when it finishes. */
export interface RunResult {
  /** The paragraph streamed into the result panel. */
  summary: string;
  /** Test totals, rendered as "12 of 12 tests passed". */
  tests?: { passed: number; total: number };
  /** Extra passing checks, such as "No type errors". */
  checks?: string[];
}

/** A file summary in the result panel. */
export interface ResultFile {
  path: string;
  additions: number;
  deletions: number;
}

/** The result panel's data, once the run is done. */
export interface RunResultView extends RunResult {
  files: ResultFile[];
}

/** A step as rendered, including its live status. */
export interface RunStep extends Omit<ScriptStep, "ms" | "revise"> {
  /** Where the step sits right now. */
  status: StepStatus;
  /** Duration in milliseconds, once the step has finished. */
  ms?: number;
  /** Why the step was sent back, for rejected approvals. */
  reason?: string;
  /** Unit progress, for steps that declare `total`. */
  progress?: { value: number; total: number };
}

/** A snapshot of the run, usable to drive the component from outside. */
export interface RunState {
  status: RunStatus;
  /** Milliseconds of simulated work, for the header timer. */
  elapsedMs: number;
  steps: RunStep[];
  /** Present only once the run is done. */
  result?: RunResultView;
}

/** A whole scripted run: its steps, its result and its pull request. */
export interface RunScript {
  steps: ScriptStep[];
  result: RunResult;
  pullRequest?: { number: number };
}

/** The model badge in the header. */
export interface AgentModel {
  /** Display name, such as "Claude Sonnet 5.5". */
  name: string;
  /** Brand mark, rendered at 14px next to the name. */
  mark?: ReactNode;
}

/** Props for `AgentRun`. */
export interface AgentRunProps {
  /** One line naming the job, shown as the card title. */
  task?: string;
  /** Model badge shown under the title. */
  model?: AgentModel;
  /** The scripted run to simulate. Defaults to the bundled sample. */
  script?: RunScript;
  /** Drive the card from your own run state instead of the simulation. */
  run?: RunState;
  /** Start the simulation as soon as the card scrolls into view. */
  autoPlay?: boolean;
  /** Called when you approve the pending change. */
  onApprove?: (change: AgentChange) => void;
  /** Called with the note when you send a change back. */
  onReject?: (change: AgentChange, reason: string) => void;
  /** Open the pull request; without it the card simulates opening one. */
  onOpenPullRequest?: () => Promise<{ number?: number; url?: string } | void> | void;
  /** Called when you restart the run. */
  onRestart?: () => void;
  /** Extra class on the card. */
  className?: string;
}

/* -------------------------------------------------------------------------------------------------
 * Sample script
 * -----------------------------------------------------------------------------------------------*/

/** Builds the file the limiter lives in; `health` adds the load balancer bypass. */
const limiterFile = (limit: string, health: boolean): ChangedFile => ({
  path: "src/middleware/rate-limit.ts",
  status: "added",
  lines: [
    'import type { RequestHandler } from "express";',
    'import { redis } from "../lib/redis";',
    "",
    "const WINDOW = 60; // seconds",
    `const LIMIT = ${limit};`,
    "",
    "export const rateLimit: RequestHandler = async (req, res, next) => {",
    ...(health ? ['  if (req.path === "/health") return next();'] : []),
    "  const key = `rate:${req.apiKey ?? req.ip}`;",
    "  const count = await redis.incr(key);",
    "  if (count === 1) await redis.expire(key, WINDOW);",
    '  res.set("RateLimit-Remaining", String(Math.max(0, LIMIT - count)));',
    "  if (count <= LIMIT) return next();",
    '  res.set("Retry-After", String(WINDOW));',
    '  res.status(429).json({ error: "Too many requests" });',
    "};",
  ].map((text, index) => ({ type: "add" as const, new: index + 1, text })),
});

/** The route file the limiter gets wired into, with its two real hunks. */
const routesFile: ChangedFile = (() => {
  let oldLine = 1;
  let newLine = 1;
  return {
    path: "src/api/routes.ts",
    status: "modified",
    lines: [
      '  import { Router } from "express";',
      '  import { requireApiKey } from "../middleware/auth";',
      '+ import { rateLimit } from "../middleware/rate-limit";',
      '  import { orders } from "./orders";',
      '  import { products } from "./products";',
      "  ",
      "  export const api = Router();",
      "  ",
      "- api.use(requireApiKey);",
      "+ api.use(requireApiKey, rateLimit);",
      '  api.use("/orders", orders);',
      '  api.use("/products", products);',
    ].map((entry) => {
      const text = entry.slice(2);
      if (entry.startsWith("+")) return { type: "add" as const, new: newLine++, text };
      if (entry.startsWith("-")) return { type: "del" as const, old: oldLine++, text };
      return { type: "ctx" as const, old: oldLine++, new: newLine++, text };
    }),
  };
})();

const firstChange: AgentChange = {
  id: "rate-limit",
  summary: "Add a Redis rate limiter, 60 requests a minute per API key, and put it in front of every API route.",
  files: [limiterFile("60", false), routesFile],
  outcome:
    "Every /api route now goes through a Redis rate limiter: 60 requests a minute per API key, falling back to the IP. Responses carry RateLimit-Remaining, and a client over the limit gets a 429 with Retry-After. Types check and all 12 API tests pass.",
};

/** The bundled demo run: rate limiting a public API, with one approval and a revision path. */
export const sampleScript: RunScript = {
  steps: [
    {
      id: "plan",
      mark: "think",
      verb: "Planning the change",
      doneVerb: "Planned the change",
      ms: 1200,
      text: "The public API has no rate limiting yet. I'll check how the routes are mounted, look for existing middleware, and see whether Redis is available so a limit holds across every instance.",
    },
    {
      id: "routes",
      mark: "read",
      verb: "Reading",
      doneVerb: "Read",
      target: "src/api/routes.ts",
      ms: 900,
      io: {
        input: "read_file src/api/routes.ts",
        output: [
          'import { Router } from "express";',
          "…",
          "export const api = Router();",
          "api.use(requireApiKey);",
          'api.use("/orders", orders);',
        ],
      },
    },
    {
      id: "search",
      mark: "search",
      verb: "Searching for",
      doneVerb: "Searched for",
      target: "rate limit middleware",
      ms: 1000,
      io: {
        input: 'grep -rni "rate.\\?limit" src',
        output: ["No matches"],
      },
    },
    {
      id: "package",
      mark: "read",
      verb: "Reading",
      doneVerb: "Read",
      target: "package.json",
      ms: 700,
      io: {
        input: "read_file package.json",
        output: ['"express": "^5.1.0",', '"ioredis": "^5.4.1",', '"zod": "^4.1.5"'],
      },
    },
    {
      id: "redis",
      mark: "read",
      verb: "Reading",
      doneVerb: "Read",
      target: "src/lib/redis.ts",
      ms: 700,
      io: {
        input: "read_file src/lib/redis.ts",
        output: ['import Redis from "ioredis";', "export const redis = new Redis(process.env.REDIS_URL!);"],
      },
    },
    {
      id: "auth",
      mark: "search",
      verb: "Searching for",
      doneVerb: "Searched for",
      target: "the API key on requests",
      ms: 900,
      io: {
        input: 'grep -rn "req.apiKey" src',
        output: ["src/middleware/auth.ts:14:  req.apiKey = key.id;", "src/types/express.d.ts:6:    apiKey?: string;"],
      },
    },
    {
      id: "review",
      mark: "review",
      verb: "Preparing",
      target: "changes",
      doneVerb: "Approved",
      doneTarget: "changes to 2 files",
      ms: 800,
      change: firstChange,
      revise: {
        steps: [
          {
            id: "revise",
            mark: "think",
            verb: "Revising the change",
            doneVerb: "Revised the change",
            ms: 1300,
            text: "Taking the note on board: the limit should be easy to change per environment, and load balancer health checks should never be limited.",
          },
        ],
        approval: {
          id: "review-2",
          mark: "review",
          verb: "Preparing",
          target: "revised changes",
          doneVerb: "Approved",
          doneTarget: "revised changes to 2 files",
          ms: 700,
          change: {
            id: "rate-limit-revised",
            summary: "Read the limit from RATE_LIMIT with a default of 120 a minute, and let health checks through.",
            files: [limiterFile("Number(process.env.RATE_LIMIT ?? 120)", true), routesFile],
            outcome:
              "Every /api route except /health now goes through a Redis rate limiter: 120 requests a minute per API key by default, set with RATE_LIMIT. Responses carry RateLimit-Remaining, and a client over the limit gets a 429 with Retry-After. Types check and all 12 API tests pass.",
          },
        },
      },
    },
    {
      id: "write",
      mark: "edit",
      verb: "Writing",
      doneVerb: "Wrote",
      target: "src/middleware/rate-limit.ts",
      ms: 900,
      io: {
        input: "write_file src/middleware/rate-limit.ts",
        output: ["Created the file"],
      },
    },
    {
      id: "wire",
      mark: "edit",
      verb: "Editing",
      doneVerb: "Edited",
      target: "src/api/routes.ts",
      ms: 700,
      io: {
        input: "edit_file src/api/routes.ts",
        output: ["Applied 1 hunk, +2 −1"],
      },
    },
    {
      id: "types",
      mark: "typescript",
      verb: "Checking types",
      doneVerb: "Checked types",
      doneTarget: "no errors",
      ms: 1100,
      io: { input: "$ npx tsc --noEmit", output: ["Found 0 errors."] },
    },
    {
      id: "tests",
      mark: "npm",
      verb: "Running tests",
      doneVerb: "Ran tests",
      doneTarget: "12 passed",
      ms: 2600,
      total: 12,
      io: {
        input: "$ npm test -- src/api",
        output: [
          "✓ orders › lists orders for the key's account",
          "✓ products › paginates with a cursor",
          "✓ rate-limit › allows requests under the limit",
          "✓ rate-limit › returns 429 with Retry-After",
          "Tests  12 passed (12)",
        ],
      },
    },
    {
      id: "summary",
      mark: "think",
      verb: "Writing the summary",
      doneVerb: "Wrote the summary",
      ms: 700,
      text: "The limiter is in, types check and the API suite passes, including two new rate limit tests.",
    },
  ],
  result: {
    summary: firstChange.outcome ?? "",
    tests: { passed: 12, total: 12 },
    checks: ["No type errors"],
  },
  pullRequest: { number: 482 },
};

/* -------------------------------------------------------------------------------------------------
 * Motion constants
 * -----------------------------------------------------------------------------------------------*/

const { spring: springTokens, duration: durationTokens } = motionTokens;
const EASE_ENTER = [...motionTokens.ease.enter] as [number, number, number, number];
const SWAP_SPRING = { type: "spring" as const, visualDuration: 0.22, bounce: 0 };

/** Shared reduced-motion read, plus the height transition to use for collapsibles. */
function useMotionPrefs() {
  const reduce = !!useReducedMotion();
  return { reduce, height: reduce ? { duration: 0 } : springTokens.smooth };
}

/* -------------------------------------------------------------------------------------------------
 * Small parts
 * -----------------------------------------------------------------------------------------------*/

interface SwapProps {
  swapKey: string;
  children: ReactNode;
  className?: string;
}

/**
 * Swaps a label in place while animating the container's width, so a row's trailing
 * content never jumps as verbs change tense or length.
 */
function Swap({ swapKey, children, className }: SwapProps) {
  const { reduce } = useMotionPrefs();
  const ref = useRef<HTMLSpanElement>(null);
  const last = useRef(0);

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el) return;
    const had = !!el.style.width;
    const before = el.getBoundingClientRect().width;
    el.style.width = "";
    const after = el.getBoundingClientRect().width;
    const from = had ? before : last.current;
    last.current = after;
    if (reduce || !from || Math.abs(from - after) < 0.5) {
      delete el.dataset.sizing;
      return;
    }
    el.dataset.sizing = "";
    const controls = animate(
      el,
      { width: [from, after] },
      {
        ...SWAP_SPRING,
        onComplete: () => {
          el.style.width = "";
          delete el.dataset.sizing;
        },
      },
    );
    return () => controls.stop();
  }, [swapKey, reduce]);

  useEffect(() => {
    const el = ref.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => {
      if (!el.style.width) last.current = el.getBoundingClientRect().width;
    });
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  return (
    <span ref={ref} className={[styles.swap, className].filter(Boolean).join(" ")}>
      <AnimatePresence mode="popLayout" initial={false}>
        <motion.span
          key={swapKey}
          className={styles.swapItem}
          initial={reduce ? { opacity: 0 } : { opacity: 0, y: 2 }}
          animate={{ opacity: 1, y: 0 }}
          exit={{ opacity: 0, transition: { duration: 0.1 * Number(!reduce) } }}
          transition={{ duration: reduce ? 0 : durationTokens.fast, ease: EASE_ENTER }}
        >
          {children}
        </motion.span>
      </AnimatePresence>
    </span>
  );
}

interface CollapseProps {
  open: boolean;
  children: ReactNode;
  id?: string;
  className?: string;
  label?: string;
}

/** A height/opacity collapsible. When `label` is set it becomes a named region. */
function Collapse({ open, children, id, className, label }: CollapseProps) {
  const { reduce, height } = useMotionPrefs();
  return (
    <AnimatePresence initial={false}>
      {open && (
        <motion.div
          key="open"
          id={id}
          role={label ? "region" : undefined}
          aria-label={label}
          className={[styles.collapse, className].filter(Boolean).join(" ")}
          initial={{ height: 0, opacity: 0 }}
          animate={{ height: "auto", opacity: 1 }}
          exit={{
            height: 0,
            opacity: 0,
            transition: { height, opacity: { duration: 0.1 * Number(!reduce) } },
          }}
          transition={{ height, opacity: { duration: reduce ? 0 : durationTokens.fast, ease: EASE_ENTER } }}
        >
          {children}
        </motion.div>
      )}
    </AnimatePresence>
  );
}

interface AutoHeightProps {
  children: ReactNode;
  className?: string;
}

/** Wraps content whose height changes on its own and springs the wrapper to match. */
function AutoHeight({ children, className }: AutoHeightProps) {
  const { height } = useMotionPrefs();
  const ref = useRef<HTMLDivElement>(null);
  const [size, setSize] = useState<number | "auto">("auto");

  useLayoutEffect(() => {
    const el = ref.current;
    if (!el || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => setSize(el.offsetHeight));
    observer.observe(el);
    return () => observer.disconnect();
  }, []);

  return (
    <motion.div className={styles.autoHeight} initial={false} animate={{ height: size }} transition={height}>
      <div ref={ref} className={className}>
        {children}
      </div>
    </motion.div>
  );
}

interface CheckProps {
  size?: number;
  className?: string;
}

/** The animated tick shown when a step finishes. */
function Check({ size = 14, className }: CheckProps) {
  const { reduce } = useMotionPrefs();
  return (
    <svg
      className={className}
      width={size}
      height={size}
      viewBox="0 0 24 24"
      fill="none"
      stroke="currentColor"
      strokeWidth={2.25}
      strokeLinecap="round"
      strokeLinejoin="round"
      aria-hidden="true"
    >
      <motion.path
        d="M4.5 12.5l5 5L19.5 7"
        initial={reduce ? false : { pathLength: 0 }}
        animate={{ pathLength: 1 }}
        transition={{ duration: 0.2, ease: EASE_ENTER }}
      />
    </svg>
  );
}

interface DiffCountProps {
  additions: number;
  deletions: number;
}

/** `+12 −3`, with the screen-reader wording spelled out. */
function DiffCount({ additions, deletions }: DiffCountProps) {
  return (
    <span className={styles.diffCount}>
      <span className={styles.add}>
        <span className={styles.srOnly}>{additions} added</span>
        <span aria-hidden="true">+{additions}</span>
      </span>
      {deletions > 0 && (
        <span className={styles.del}>
          <span className={styles.srOnly}>, {deletions} removed</span>
          <span aria-hidden="true">−{deletions}</span>
        </span>
      )}
    </span>
  );
}

interface DiffProps {
  change: AgentChange;
}

/** The changed-file tabs and their unified diff panels. */
function Diff({ change }: DiffProps) {
  const base = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const [active, setActive] = useState(0);
  const tabs = useRef<Array<HTMLButtonElement | null>>([]);
  const files = change.files;

  const onKeyDown = (event: React.KeyboardEvent<HTMLButtonElement>) => {
    const last = files.length - 1;
    const next =
      event.key === "ArrowRight"
        ? active === last
          ? 0
          : active + 1
        : event.key === "ArrowLeft"
          ? active === 0
            ? last
            : active - 1
          : event.key === "Home"
            ? 0
            : event.key === "End"
              ? last
              : -1;
    if (next < 0) return;
    event.preventDefault();
    setActive(next);
    tabs.current[next]?.focus();
  };

  return (
    <div>
      <div className={styles.fileTabs} role="tablist" aria-label="Changed files">
        {files.map((file, index) => (
          <button
            key={file.path}
            ref={(node) => {
              tabs.current[index] = node;
            }}
            type="button"
            role="tab"
            id={`${base}-tab-${index}`}
            className={styles.fileTab}
            aria-selected={index === active}
            aria-controls={`${base}-panel-${index}`}
            tabIndex={index === active ? 0 : -1}
            title={file.path}
            onClick={() => setActive(index)}
            onKeyDown={onKeyDown}
          >
            <span className={styles.fileName}>{file.path.split("/").pop() ?? file.path}</span>
            <DiffCount
              additions={file.lines.filter((line) => line.type === "add").length}
              deletions={file.lines.filter((line) => line.type === "del").length}
            />
          </button>
        ))}
      </div>
      <div className={styles.panels}>
        {files.map((file, index) => (
          <div
            key={file.path}
            role="tabpanel"
            id={`${base}-panel-${index}`}
            aria-labelledby={`${base}-tab-${index}`}
            aria-label={file.path}
            className={styles.panel}
            data-active={index === active || undefined}
            inert={index !== active}
            tabIndex={index === active ? 0 : -1}
          >
            <div className={styles.code}>
              {file.lines.map((line, lineIndex) => (
                <div key={lineIndex} className={styles.line} data-type={line.type}>
                  <span className={styles.num} aria-hidden="true">
                    {line.type === "del" ? line.old : line.new}
                  </span>
                  <span className={styles.marker} aria-hidden="true">
                    {line.type === "add" ? "+" : line.type === "del" ? "−" : ""}
                  </span>
                  <code className={styles.src}>
                    {line.type !== "ctx" && (
                      <span className={styles.srOnly}>{line.type === "add" ? "Added: " : "Removed: "}</span>
                    )}
                    {line.text || " "}
                  </code>
                </div>
              ))}
            </div>
          </div>
        ))}
      </div>
    </div>
  );
}

interface ChangeBodyProps {
  change: AgentChange;
  rejected: boolean;
  reason?: string;
}

/** A settled change: its summary, its diff and your verdict. */
function ChangeBody({ change, rejected, reason }: ChangeBodyProps) {
  return (
    <div className={styles.changeBody}>
      <p className={styles.changeSummary}>{change.summary}</p>
      <div className={styles.diffCard}>
        <Diff change={change} />
      </div>
      <p className={styles.verdict} data-tone={rejected ? "rejected" : "approved"}>
        {rejected ? <CornerUpLeft size={14} strokeWidth={2} aria-hidden="true" /> : <CircleCheck size={14} strokeWidth={2} aria-hidden="true" />}
        {rejected ? (
          <>
            Sent back
            {reason ? (
              <>
                : <q>{reason}</q>
              </>
            ) : null}
          </>
        ) : (
          "Approved by you"
        )}
      </p>
    </div>
  );
}

interface DecideProps {
  change: AgentChange;
  onApprove?: () => void;
  onReject?: (reason: string) => void;
}

/** The approval gate: approve, or send the change back with a note. */
function Decide({ change, onApprove, onReject }: DecideProps) {
  const { reduce } = useMotionPrefs();
  const [phase, setPhase] = useState<"ask" | "reason">("ask");
  const [note, setNote] = useState("");
  const inputRef = useRef<HTMLInputElement>(null);
  const rejectRef = useRef<HTMLButtonElement>(null);
  const returning = useRef(false);

  useEffect(() => {
    if (phase === "reason") inputRef.current?.focus({ preventScroll: true });
    else if (returning.current) {
      returning.current = false;
      rejectRef.current?.focus({ preventScroll: true });
    }
  }, [phase]);

  const cancel = () => {
    returning.current = true;
    setPhase("ask");
  };

  const stage = {
    initial: reduce ? { opacity: 0 } : { opacity: 0, y: 2 },
    animate: { opacity: 1, y: 0 },
    exit: { opacity: 0, transition: { duration: 0.1 * Number(!reduce) } },
    transition: { duration: 0.16 * Number(!reduce), ease: EASE_ENTER },
  };

  return (
    <div className={styles.changeBody}>
      <p className={styles.changeSummary}>{change.summary}</p>
      <div className={styles.diffCard}>
        <Diff change={change} />
        <AutoHeight className={styles.decideStage}>
          <AnimatePresence mode="popLayout" initial={false}>
            {phase === "ask" ? (
              <motion.div key="ask" className={styles.decide} {...stage}>
                <p className={styles.decideNote}>Nothing is written until you approve.</p>
                <div className={styles.decideActions}>
                  <Button ref={rejectRef} variant="secondary" size="sm" onClick={() => setPhase("reason")}>
                    Reject
                  </Button>
                  <Button size="sm" className={styles.accentButton} onClick={onApprove}>
                    Approve
                  </Button>
                </div>
              </motion.div>
            ) : (
              <motion.form
                key="reason"
                className={styles.reasonForm}
                onSubmit={(event) => {
                  event.preventDefault();
                  const trimmed = note.trim();
                  if (trimmed) onReject?.(trimmed);
                }}
                {...stage}
                onKeyDown={(event) => {
                  if (event.key === "Escape") {
                    event.preventDefault();
                    cancel();
                  }
                }}
              >
                <Input
                  ref={inputRef}
                  label="What should change?"
                  placeholder="For example, make the limit configurable"
                  value={note}
                  onChange={(event: React.ChangeEvent<HTMLInputElement>) => setNote(event.target.value)}
                  autoComplete="off"
                />
                <div className={styles.decideActions}>
                  <Button type="button" variant="ghost" size="sm" onClick={cancel}>
                    Cancel
                  </Button>
                  <Button type="submit" size="sm" className={styles.accentButton} disabled={!note.trim()}>
                    Send back
                  </Button>
                </div>
              </motion.form>
            )}
          </AnimatePresence>
        </AutoHeight>
      </div>
    </div>
  );
}

/** Word-group sizes the summary streams out in. */
const CHUNKS = [4, 6, 3, 5, 7, 4, 5];

interface StreamedTextProps {
  text: string;
  className?: string;
}

/** Reveals a paragraph group by group, as if the model were still writing it. */
function StreamedText({ text, className }: StreamedTextProps) {
  const { reduce } = useMotionPrefs();
  const groups = useMemo(() => {
    const words = text.split(" ");
    const out: string[] = [];
    for (let index = 0, group = 0; index < words.length; group++) {
      const size = CHUNKS[group % CHUNKS.length];
      out.push(words.slice(index, index + size).join(" ") + (index + size < words.length ? " " : ""));
      index += size;
    }
    return out;
  }, [text]);
  const [shown, setShown] = useState(reduce ? groups.length : 0);

  useEffect(() => {
    if (reduce) return;
    let frame = 0;
    let start = 0;
    const tick = (now: number) => {
      if (!start) start = now;
      const next = Math.min(groups.length, 1 + Math.floor((now - start) / 34));
      setShown(next);
      if (next < groups.length) frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  }, [groups, reduce]);

  return (
    <p className={className}>
      {groups.map((group, index) => (
        <span key={index} className={styles.chunk} data-shown={reduce || index < shown || undefined}>
          {group}
        </span>
      ))}
    </p>
  );
}

interface DiffBarProps {
  additions: number;
  deletions: number;
}

/** A five cell additive/removal bar, proportional to the counts. */
function DiffBar({ additions, deletions }: DiffBarProps) {
  const total = additions + deletions || 1;
  const add = Math.round((5 * additions) / total);
  const del = Math.min(5 - add, Math.round((5 * deletions) / total));
  return (
    <span className={styles.diffBar} aria-hidden="true">
      {Array.from({ length: 5 }, (_, index) => (
        <span key={index} data-kind={index < add ? "add" : index < add + del ? "del" : undefined} />
      ))}
    </span>
  );
}

interface PrLabelProps {
  state: "idle" | "opening" | "opened";
  number?: number;
}

/** The pull request button's three stacked labels. */
function PrLabel({ state, number }: PrLabelProps) {
  return (
    <span className={styles.labelStack}>
      <span data-on={state === "idle" || undefined} aria-hidden={state !== "idle" || undefined}>
        <GitHubMark className={styles.ghMark} aria-hidden="true" />
        Open pull request
      </span>
      <span data-on={state === "opening" || undefined} aria-hidden={state !== "opening" || undefined}>
        <LoaderCircle className={styles.spin} size={16} strokeWidth={1.75} aria-hidden="true" />
        Opening
      </span>
      <span data-on={state === "opened" || undefined} aria-hidden={state !== "opened" || undefined}>
        <GitHubMark className={styles.ghMark} aria-hidden="true" />
        PR #{number ?? ""} opened
      </span>
    </span>
  );
}

interface ResultPanelProps {
  result: RunResultView;
  pr: "idle" | "opening" | "opened";
  prNumber?: number;
  onOpenPr: () => void;
}

/** The finished run: outcome, files, checks and the pull request button. */
function ResultPanel({ result, pr, prNumber, onOpenPr }: ResultPanelProps) {
  const additions = result.files.reduce((sum, file) => sum + file.additions, 0);
  const deletions = result.files.reduce((sum, file) => sum + file.deletions, 0);
  const count = result.files.length;

  return (
    <div className={styles.result}>
      <StreamedText className={styles.summary} text={result.summary} />
      {count > 0 && (
        <div className={styles.files}>
          <p className={styles.filesHead}>
            <span>
              {count} {count === 1 ? "file" : "files"} changed
            </span>
            <DiffCount additions={additions} deletions={deletions} />
          </p>
          <ul className={styles.fileList} aria-label="Files changed">
            {result.files.map((file) => (
              <li key={file.path} className={styles.fileRow}>
                <FileCodeCorner className={styles.fileIcon} size={16} strokeWidth={1.75} aria-hidden="true" />
                <span className={styles.filePath} title={file.path}>
                  {file.path}
                </span>
                <DiffCount additions={file.additions} deletions={file.deletions} />
                <DiffBar additions={file.additions} deletions={file.deletions} />
              </li>
            ))}
          </ul>
        </div>
      )}
      {(result.tests || result.checks?.length) && (
        <ul className={styles.checks} aria-label="Checks">
          {result.tests && (
            <li>
              <CircleCheck size={16} strokeWidth={1.75} aria-hidden="true" />
              {result.tests.passed} of {result.tests.total} tests passed
            </li>
          )}
          {result.checks?.map((check) => (
            <li key={check}>
              <CircleCheck size={16} strokeWidth={1.75} aria-hidden="true" />
              {check}
            </li>
          ))}
        </ul>
      )}
      <div className={styles.resultActions}>
        <Button size="sm" className={styles.prButton} data-pr={pr} aria-disabled={pr !== "idle" || undefined} onClick={onOpenPr}>
          <PrLabel state={pr} number={prNumber} />
        </Button>
      </div>
    </div>
  );
}

interface StoppedPanelProps {
  onRestart: () => void;
}

/** The terminal panel for a run that was sent back twice. */
function StoppedPanel({ onRestart }: StoppedPanelProps) {
  return (
    <div className={styles.result}>
      <p className={styles.summary}>The change was sent back twice, so the agent stopped. No files were written.</p>
      <div className={styles.resultActions}>
        <Button size="sm" variant="secondary" onClick={onRestart}>
          <RotateCcw size={16} strokeWidth={1.75} aria-hidden="true" />
          Run again
        </Button>
      </div>
    </div>
  );
}

/* -------------------------------------------------------------------------------------------------
 * Step icons
 * -----------------------------------------------------------------------------------------------*/

const Sparkle = createLucideIcon("Sparkle", [
  [
    "path",
    {
      d: "M11.017 2.814a1 1 0 0 1 1.966 0l1.051 5.558a2 2 0 0 0 1.594 1.594l5.558 1.051a1 1 0 0 1 0 1.966l-5.558 1.051a2 2 0 0 0-1.594 1.594l-1.051 5.558a1 1 0 0 1-1.966 0l-1.051-5.558a2 2 0 0 0-1.594-1.594l-5.558-1.051a1 1 0 0 1 0-1.966l5.558-1.051a2 2 0 0 0 1.594-1.594z",
      key: "1s2grr",
    },
  ],
]);

const MARKS = {
  think: Sparkle,
  read: FileText,
  search: Search,
  edit: FilePen,
  terminal: Terminal,
  review: FileDiff,
} as const;

interface MarkIconProps {
  mark: AgentMark;
}

/** Renders the icon for a step's `mark`, including the brand marks. */
function MarkIcon({ mark }: MarkIconProps) {
  if (mark === "npm") return <NpmMark className={styles.brandMark} data-brand="npm" aria-hidden="true" />;
  if (mark === "typescript") return <TypeScriptMark className={styles.brandMark} data-brand="typescript" aria-hidden="true" />;
  if (mark === "github") return <GitHubMark className={styles.brandMark} aria-hidden="true" />;
  const Icon = MARKS[mark];
  return <Icon size={16} strokeWidth={1.75} aria-hidden="true" />;
}

/* -------------------------------------------------------------------------------------------------
 * Step row
 * -----------------------------------------------------------------------------------------------*/

/** Resolves the verb and target a step row shows for its current status. */
function label(step: RunStep): { verb: string; target?: string } {
  if (step.status === "running") return { verb: step.verb, target: step.target };
  if (step.status === "waiting") return { verb: "Review", target: step.doneTarget ?? step.target };
  if (step.status === "rejected") return { verb: "Sent back", target: step.doneTarget ?? step.target };
  return { verb: step.doneVerb, target: step.doneTarget ?? step.target };
}

interface StepDetailsProps {
  step: RunStep;
}

/** A step's expanded body: a change, or a thought plus its tool exchange. */
function StepDetails({ step }: StepDetailsProps) {
  if (step.change) {
    return <ChangeBody change={step.change} rejected={step.status === "rejected"} reason={step.reason} />;
  }
  return (
    <div className={styles.details}>
      {step.text && <p className={styles.thought}>{step.text}</p>}
      {step.io && (
        <div className={styles.io}>
          <code className={styles.ioInput}>{step.io.input}</code>
          <div className={styles.ioOutput}>
            {step.io.output.map((line, index) => (
              <code key={index}>{line}</code>
            ))}
          </div>
        </div>
      )}
    </div>
  );
}

interface StepGate {
  onApprove: () => void;
  onReject: (reason: string) => void;
}

interface StepRowProps {
  step: RunStep;
  open: boolean;
  headId: string;
  onToggle: (id: string) => void;
  gate?: StepGate;
}

/** One row of the timeline: a head button and its collapsible body. */
const StepRow = memo(function StepRow({ step, open, headId, onToggle, gate }: StepRowProps) {
  const { reduce, height } = useMotionPrefs();
  const { verb, target } = label(step);
  const gated = step.status === "waiting" && gate && step.change ? gate : undefined;
  const expandable = !gated && !!(step.text || step.io || step.change);
  const detailsId = `${headId}-details`;

  const head = (
    <>
      <span className={styles.stepIcon}>{step.icon ?? <MarkIcon mark={step.mark} />}</span>
      <span className={styles.stepLabel}>
        <Swap swapKey={`${step.status}:${verb}`} className={styles.verb}>
          {step.status === "running" ? <TextShimmer>{verb}</TextShimmer> : verb}
        </Swap>
        {target &&
          (step.status === "rejected" ? (
            <span className={styles.target}>
              <q>{step.reason ?? target}</q>
            </span>
          ) : (
            <span className={styles.target}>{target}</span>
          ))}
        {step.status === "running" && step.progress && (
          <span className={styles.count}>
            <span className={styles.countValue} style={{ minWidth: `${String(step.progress.total).length}ch` }}>
              {step.progress.value}
            </span>{" "}
            of {step.progress.total}
          </span>
        )}
      </span>
      <span className={styles.stepTime}>
        {step.status === "done" && step.ms ? (step.ms < 10_000 ? `${(step.ms / 1_000).toFixed(1)}s` : `${Math.round(step.ms / 1_000)}s`) : ""}
      </span>
      <span className={styles.stepState} aria-hidden="true">
        {step.status === "done" && <Check />}
        {step.status === "rejected" && <CornerUpLeft size={14} strokeWidth={2} />}
      </span>
      <span className={styles.chevronSlot} aria-hidden="true">
        {expandable && <ChevronDown className={styles.chevron} size={16} strokeWidth={1.75} />}
      </span>
    </>
  );

  return (
    <motion.li
      className={styles.step}
      data-status={step.status}
      initial={reduce ? { opacity: 0 } : { opacity: 0, height: 0 }}
      animate={{ opacity: 1, height: "auto" }}
      exit={
        reduce
          ? { opacity: 0, transition: { duration: 0 } }
          : { opacity: 0, height: 0, transition: { height, opacity: { duration: 0.1 } } }
      }
      transition={{ height, opacity: { duration: 0.16 * Number(!reduce), ease: EASE_ENTER } }}
    >
      {expandable ? (
        <button
          type="button"
          id={headId}
          className={styles.stepHead}
          aria-expanded={open}
          aria-controls={open ? detailsId : undefined}
          onClick={() => onToggle(step.id)}
        >
          {head}
        </button>
      ) : (
        <div id={headId} className={styles.stepHead} tabIndex={-1}>
          {head}
        </div>
      )}
      <Collapse open={!!gated || (expandable && open)} id={detailsId}>
        <div className={styles.stepBody}>
          {gated && step.change ? (
            <Decide key={step.id} change={step.change} onApprove={gated.onApprove} onReject={gated.onReject} />
          ) : (
            <StepDetails step={step} />
          )}
        </div>
      </Collapse>
    </motion.li>
  );
});

/* -------------------------------------------------------------------------------------------------
 * Simulation
 * -----------------------------------------------------------------------------------------------*/

const isApproval = (step: ScriptStep): step is ScriptStep & { change: AgentChange } => "change" in step;

/** Runs a `ScriptStep` for rendering, in its running shape. */
function asRunning(step: ScriptStep): RunStep {
  const { id, mark, verb, doneVerb, target, doneTarget, io, text, icon } = step;
  return {
    id,
    mark,
    verb,
    doneVerb,
    target,
    doneTarget,
    io,
    text,
    icon,
    status: "running",
    change: isApproval(step) ? step.change : undefined,
  };
}

interface SimState {
  phase: RunStatus;
  plan: ScriptStep[];
  index: number;
  clock: number;
  elapsed: number;
  finished: RunStep[];
  revised: boolean;
  approved?: AgentChange;
  result: RunResult;
}

const initialSim = (script: RunScript, phase: RunStatus = "idle"): SimState => ({
  phase,
  plan: script.steps,
  index: 0,
  clock: 0,
  elapsed: 0,
  finished: [],
  revised: false,
  result: script.result,
});

/** Advances the clock by `delta` ms, consuming finished steps and stopping at the next approval. */
function advance(state: SimState, delta: number): SimState {
  if (state.phase !== "working") return state;
  let { index, clock, finished } = state;
  clock += delta;
  const elapsed = state.elapsed + delta;
  while (index < state.plan.length) {
    const step = state.plan[index];
    if (clock < step.ms) break;
    if (isApproval(step)) {
      return { ...state, finished, index, clock: step.ms, elapsed: elapsed - (clock - step.ms), phase: "waiting" };
    }
    finished = [...finished, { ...asRunning(step), status: "done", ms: step.ms }];
    clock -= step.ms;
    index += 1;
  }
  if (index >= state.plan.length) {
    return { ...state, finished, index, clock: 0, elapsed: elapsed - clock, phase: "done" };
  }
  return { ...state, finished, index, clock, elapsed };
}

/**
 * Drives a scripted run on an animation frame clock.
 *
 * Time only advances while `running` is true, in capped 40 ms buckets, so a backgrounded tab
 * cannot skip a step. The run parks in `waiting` whenever the next step needs your approval.
 */
export function useSimulatedRun(script: RunScript, { running }: { running: boolean }) {
  const latest = useRef(script);
  useEffect(() => {
    latest.current = script;
  });
  const [state, setState] = useState<SimState>(() => initialSim(script));
  const ticking = running && state.phase === "working";

  useEffect(() => {
    if (!ticking) return;
    let frame = 0;
    let last = performance.now();
    let carry = 0;
    const tick = (now: number) => {
      carry += Math.min(now - last, 100);
      last = now;
      if (carry >= 40) {
        const delta = carry;
        carry = 0;
        setState((current) => advance(current, delta));
      }
      frame = requestAnimationFrame(tick);
    };
    frame = requestAnimationFrame(tick);
    return () => cancelAnimationFrame(frame);
  }, [ticking]);

  const start = useCallback(() => setState((current) => (current.phase === "idle" ? { ...current, phase: "working" } : current)), []);

  const restart = useCallback(() => setState(initialSim(latest.current, "working")), []);

  const approve = useCallback(
    () =>
      setState((current) => {
        const step = current.plan[current.index];
        if (current.phase !== "waiting" || !step || !isApproval(step)) return current;
        return {
          ...current,
          phase: "working",
          index: current.index + 1,
          clock: 0,
          approved: step.change,
          finished: [...current.finished, { ...asRunning(step), status: "done", ms: step.ms }],
        };
      }),
    [],
  );

  const reject = useCallback(
    (reason: string) =>
      setState((current) => {
        const step = current.plan[current.index];
        if (current.phase !== "waiting" || !step || !isApproval(step)) return current;
        const finished = [...current.finished, { ...asRunning(step), status: "rejected" as const, ms: step.ms, reason }];
        if (!step.revise || current.revised) {
          return { ...current, phase: "stopped", finished, index: current.index + 1, clock: 0 };
        }
        const inserted = step.revise.steps.map((entry, position) =>
          position === 0 && entry.text ? { ...entry, text: `“${reason}” ${entry.text}` } : entry,
        );
        const plan = [
          ...current.plan.slice(0, current.index + 1),
          ...inserted,
          step.revise.approval,
          ...current.plan.slice(current.index + 1),
        ];
        return { ...current, phase: "working", plan, finished, index: current.index + 1, clock: 0, revised: true };
      }),
    [],
  );

  const view = useMemo<RunState>(() => {
    const current = state.plan[state.index];
    const steps = [...state.finished];
    if (current && (state.phase === "working" || state.phase === "waiting")) {
      const running = asRunning(current);
      if (state.phase === "waiting") running.status = "waiting";
      else if (current.total) {
        running.progress = {
          value: Math.min(current.total, 1 + Math.floor((state.clock / current.ms) * current.total)),
          total: current.total,
        };
      }
      steps.push(running);
    }
    const result = state.result;
    return {
      status: state.phase,
      elapsedMs: state.elapsed,
      steps,
      result:
        state.phase === "done"
          ? {
              ...result,
              summary: state.approved?.outcome ?? result.summary,
              files: state.approved
                ? state.approved.files.map((file) => ({
                    path: file.path,
                    additions: file.lines.filter((line) => line.type === "add").length,
                    deletions: file.lines.filter((line) => line.type === "del").length,
                  }))
                : [],
            }
          : undefined,
    };
  }, [state]);

  return { state: view, start, restart, approve, reject };
}

/* -------------------------------------------------------------------------------------------------
 * AgentRun
 * -----------------------------------------------------------------------------------------------*/

const STATUS_TEXT: Record<RunStatus, string> = {
  idle: "Ready",
  working: "Working",
  waiting: "Waiting for you",
  done: "Done",
  stopped: "Stopped",
};

/** Default model badge. */
const DEFAULT_MODEL: AgentModel = {
  name: "Claude Sonnet 5.5",
  mark: <ClaudeMark aria-hidden="true" />,
};

/** `m:ss`, for the header timer. */
const clock = (ms: number) => {
  const seconds = Math.floor(ms / 1_000);
  return `${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, "0")}`;
};

/** `45s` / `1m 12s`, for the fold and the done announcement. */
const shortDuration = (ms: number) => {
  const seconds = Math.max(1, Math.round(ms / 1_000));
  return seconds < 60 ? `${seconds}s` : `${Math.floor(seconds / 60)}m ${String(seconds % 60).padStart(2, "0")}s`;
};

interface StatusIconProps {
  status: RunStatus;
}

/** The status glyph beside the status label. */
function StatusIcon({ status }: StatusIconProps) {
  const shared = { className: styles.statusIcon, size: 14, strokeWidth: 2, "aria-hidden": true } as const;
  if (status === "working") return <LoaderCircle {...shared} className={`${styles.statusIcon} ${styles.spin}`} />;
  if (status === "waiting") return <Hand {...shared} />;
  if (status === "done") return <CircleCheck {...shared} />;
  if (status === "stopped") return <CircleMinus {...shared} />;
  return <Circle {...shared} />;
}

/**
 * An agent working through a task, as a running timeline of steps with tool details, an approval
 * gate, an elapsed timer and a result panel.
 *
 * The run plays on its own until it needs your approval; approving writes the change into the
 * result, sending it back with a note revises it once, and a second rejection stops the run.
 */
export function AgentRun({
  task = "Add rate limiting to the public API",
  model = DEFAULT_MODEL,
  script = sampleScript,
  run,
  autoPlay = true,
  onApprove,
  onReject,
  onOpenPullRequest,
  onRestart,
  className,
}: AgentRunProps) {
  const base = useId().replace(/[^a-zA-Z0-9_-]/g, "");
  const ref = useRef<HTMLElement>(null);
  const inView = useInView(ref, { amount: 0.2 });
  const { reduce, height } = useMotionPrefs();
  const simulation = useSimulatedRun(script, { running: !run && inView });
  const { start } = simulation;
  const state = run ?? simulation.state;
  const { status, steps, result } = state;
  const collapsed = status === "done" || status === "stopped";

  const [openSteps, setOpenSteps] = useState<Record<string, boolean>>({});
  const [folded, setFolded] = useState(false);
  const [pr, setPr] = useState<{ state: "idle" | "opening" | "opened"; number?: number; url?: string }>({ state: "idle" });
  const [message, setMessage] = useState("");
  const prTimer = useRef<number | undefined>(undefined);

  useEffect(() => {
    if (!run && autoPlay && inView) start();
  }, [run, autoPlay, inView, start]);

  useEffect(() => () => window.clearTimeout(prTimer.current), []);

  const headId = useCallback((id: string) => `${base}-step-${id}`, [base]);

  const focusHead = useCallback(
    (id: string) => {
      requestAnimationFrame(() => document.getElementById(headId(id))?.focus({ preventScroll: true }));
    },
    [headId],
  );

  const toggle = useCallback((id: string) => setOpenSteps((current) => ({ ...current, [id]: !current[id] })), []);

  const pending = steps.find((step) => step.status === "waiting" && step.change);
  const pendingRef = useRef(pending);

  useEffect(() => {
    pendingRef.current = pending;
  });

  const { approve, reject, restart } = simulation;
  const external = !!run;

  const handleApprove = useCallback(() => {
    const step = pendingRef.current;
    if (!step?.change) return;
    onApprove?.(step.change);
    if (!external) approve();
    focusHead(step.id);
  }, [onApprove, external, approve, focusHead]);

  const handleReject = useCallback(
    (reason: string) => {
      const step = pendingRef.current;
      if (!step?.change) return;
      onReject?.(step.change, reason);
      if (!external) reject(reason);
      focusHead(step.id);
    },
    [onReject, external, reject, focusHead],
  );

  const gate = useMemo<StepGate>(() => ({ onApprove: handleApprove, onReject: handleReject }), [handleApprove, handleReject]);

  const handleRestart = () => {
    window.clearTimeout(prTimer.current);
    setOpenSteps({});
    setFolded(false);
    setPr({ state: "idle" });
    setMessage("");
    onRestart?.();
    if (!external) restart();
  };

  const handleOpenPr = async () => {
    if (pr.state === "opened" && pr.url) {
      window.open(pr.url, "_blank", "noopener,noreferrer");
      return;
    }
    if (pr.state !== "idle") return;
    setPr({ state: "opening" });
    setMessage("");
    if (!onOpenPullRequest) {
      prTimer.current = window.setTimeout(() => {
        const number = script.pullRequest?.number ?? 482;
        setPr({ state: "opened", number });
        setMessage(`Simulated: pull request #${number} opened. Nothing was sent to GitHub.`);
      }, 900);
      return;
    }
    try {
      const opened = await onOpenPullRequest();
      const number = opened?.number ?? script.pullRequest?.number;
      setPr({ state: "opened", number, url: opened?.url });
      setMessage(`Pull request${number ? ` #${number}` : ""} opened.`);
    } catch {
      setPr({ state: "idle" });
      setMessage("The pull request could not be opened. Try again.");
    }
  };

  const last = steps[steps.length - 1];
  const announcement =
    status === "working" && last?.status === "running"
      ? (() => {
          const { verb, target } = label(last);
          return [verb, target].filter(Boolean).join(" ");
        })()
      : status === "waiting" && pending?.change
        ? `Waiting for you. Review the changes to ${pending.change.files.length} files, then approve or reject.`
        : status === "done"
          ? `Done in ${shortDuration(state.elapsedMs)}.${result?.tests ? ` ${result.tests.passed} of ${result.tests.total} tests passed.` : ""}`
          : status === "stopped"
            ? "Stopped. No files were written."
            : "";

  const visible = !collapsed || folded;
  const stepsId = `${base}-steps`;

  return (
    <section ref={ref} className={[styles.root, className].filter(Boolean).join(" ")} aria-labelledby={`${base}-task`}>
      <header className={styles.header}>
        <div className={styles.titleRow}>
          <h3 id={`${base}-task`} className={styles.task}>
            {task}
          </h3>
          <Tooltip content="Restart run">
            <Button variant="ghost" size="sm" className={styles.iconButton} aria-label="Restart run" onClick={handleRestart}>
              <RotateCcw size={16} strokeWidth={1.75} aria-hidden="true" />
            </Button>
          </Tooltip>
        </div>
        <div className={styles.metaRow}>
          <span className={styles.model}>
            {model.mark && <span className={styles.modelMark}>{model.mark}</span>}
            {model.name}
          </span>
          <span className={styles.statusLine}>
            <span className={styles.status} data-status={status}>
              <Swap swapKey={status}>
                <StatusIcon status={status} />
                {status === "working" ? <TextShimmer>{STATUS_TEXT.working}</TextShimmer> : STATUS_TEXT[status]}
              </Swap>
            </span>
            <span className={styles.timer} role="timer" aria-label={`Worked for ${clock(state.elapsedMs)}`}>
              {clock(state.elapsedMs)}
            </span>
          </span>
        </div>
      </header>

      <div className={styles.stepsArea}>
        <Collapse open={collapsed}>
          <button
            type="button"
            className={styles.fold}
            aria-expanded={folded}
            aria-controls={stepsId}
            onClick={() => setFolded((current) => !current)}
          >
            <ListChecks className={styles.stepIcon} size={16} strokeWidth={1.75} aria-hidden="true" />
            <span>
              {steps.length} steps · {shortDuration(state.elapsedMs)}
            </span>
            <ChevronDown className={styles.chevron} size={16} strokeWidth={1.75} aria-hidden="true" />
          </button>
        </Collapse>
        <motion.div
          id={stepsId}
          className={styles.listWrap}
          initial={false}
          inert={!visible}
          animate={{ height: visible ? "auto" : 0, opacity: Number(visible) }}
          transition={{ height, opacity: { duration: reduce ? 0 : visible ? 0.16 : 0.1 } }}
        >
          <ol className={styles.steps} aria-label="Steps">
            <AnimatePresence initial={false}>
              {steps.map((step) => (
                <StepRow
                  key={step.id}
                  step={step}
                  open={!!openSteps[step.id]}
                  headId={headId(step.id)}
                  onToggle={toggle}
                  gate={step.status === "waiting" ? gate : undefined}
                />
              ))}
            </AnimatePresence>
          </ol>
        </motion.div>
        {status === "idle" && !autoPlay && !run && (
          <div className={styles.ready}>
            <p>The agent is ready to start.</p>
            <Button size="sm" className={styles.accentButton} onClick={start}>
              <Play size={16} strokeWidth={1.75} aria-hidden="true" />
              Start run
            </Button>
          </div>
        )}
      </div>

      <Collapse open={status === "done" && !!result} label="Result">
        {result && <ResultPanel result={result} pr={pr.state} prNumber={pr.number} onOpenPr={handleOpenPr} />}
      </Collapse>

      <Collapse open={status === "stopped"} label="Run stopped">
        <StoppedPanel onRestart={handleRestart} />
      </Collapse>

      <p className={styles.srOnly} aria-live="polite">
        {announcement}
      </p>
      <p className={styles.srOnly} aria-live="polite">
        {message}
      </p>
    </section>
  );
}

export default AgentRun;
