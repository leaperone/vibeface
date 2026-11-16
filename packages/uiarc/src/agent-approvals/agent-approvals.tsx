"use client";

import { forwardRef, useCallback, useEffect, useId, useLayoutEffect, useRef, useState, useSyncExternalStore } from "react";
import type { KeyboardEvent as ReactKeyboardEvent, ReactElement, ReactNode, RefObject, SVGProps } from "react";
import { AnimatePresence, motion, useReducedMotion } from "motion/react";
import type { LucideIcon } from "lucide-react";
import { Bot, Building2, Check, CheckCheck, ChevronRight, CircleAlert, Pencil, RotateCcw, ShieldAlert, ShieldCheck, ShieldX, User, Wrench, X } from "lucide-react";
import { AnimatedCounter } from "../animated-counter/animated-counter";
import { Button } from "../button/button";
import { DropdownMenu } from "../dropdown-menu/dropdown-menu";
import { Popover, PopoverContent, PopoverTrigger } from "../popover/popover";
import { TextShimmer } from "../text-shimmer/text-shimmer";
import { motionTokens } from "../lib/motion-tokens";
import styles from "./agent-approvals.module.css";

export type ApprovalsRisk = "low" | "medium" | "high";
export type AgentProvider = "anthropic" | "openai" | "google";
export type ToolBrand = "vercel" | "gmail" | "postgresql" | "linear" | "github" | "sentry" | "stripe";

export interface ApprovalsAgent {
  id: string;
  name: string;
  model: string;
  provider?: AgentProvider;
}

export interface ApprovalsTool {
  id: string;
  name: string;
  action: string;
  brand?: ToolBrand;
}

export interface ApprovalArg {
  key: string;
  label: string;
  value: string;
  /** Values start editable; set false for values a person must not rewrite, such as a database name. */
  editable?: boolean;
  /** Render the value in the mono face, for ids, commands and code. */
  mono?: boolean;
  /** Open the editor as a textarea, so long values keep their line breaks. */
  multiline?: boolean;
}

export interface ApprovalRequest {
  id: string;
  agent: ApprovalsAgent;
  tool: ApprovalsTool;
  /** Sentence case title; text wrapped in backticks renders as code. */
  title: string;
  summary: string;
  reason: string;
  args: ApprovalArg[];
  risk: ApprovalsRisk;
  /** ISO timestamp for when the agent asked. */
  requestedAt: string;
}

export interface ApprovalRule {
  id: string;
  tool: ApprovalsTool;
  scope: "agent" | "workspace";
  agent?: { id: string; name: string };
  createdBy?: string;
  createdAt?: string;
}

export interface ResolvedEntry {
  id: string;
  agentName: string;
  tool: ApprovalsTool;
  summary: string;
  outcome: "approved" | "denied" | "allowed";
  edited?: boolean;
  decidedBy: string;
  decidedAt: string;
}

export interface ApprovalDecision {
  requestId: string;
  outcome: "approved" | "denied" | "allowed";
  args: Record<string, string>;
  edited: boolean;
  ruleId?: string;
  decidedBy: string;
  decidedAt: string;
}

export interface AgentApprovalsProps {
  /** Waiting requests, newest first. Leave out to run the built in sample queue. */
  requests?: ApprovalRequest[];
  rules?: ApprovalRule[];
  resolved?: ResolvedEntry[];
  /** Name that signs every decision in the audit log. */
  reviewer?: string;
  workspace?: string;
  workspaceMark?: ReactNode;
  title?: string;
  onDecision?: (decision: ApprovalDecision) => void | Promise<unknown>;
  onRuleChange?: (change: { type: "added" | "removed"; rule: ApprovalRule }) => void;
  className?: string;
}

type DecideOutcome = "approved" | "denied";

type CardState =
  | { phase: "idle" }
  | { phase: "sending"; outcome: DecideOutcome }
  | { phase: "done"; outcome: DecideOutcome; edited?: boolean; rule?: ApprovalRule }
  | { phase: "error"; outcome: DecideOutcome; message?: string }
  | { phase: "auto"; outcome: "allowed"; rule: ApprovalRule };

type Settlement = { outcome: "approved" | "denied" | "allowed"; edited?: boolean; decidedBy: string; decidedAt: string };

const IDLE: CardState = { phase: "idle" };
type Bezier = [number, number, number, number];
const ENTER: Bezier = [...motionTokens.ease.enter];
const EXIT_EASE: Bezier = [...motionTokens.ease.standard];
const FLIGHT_EASE: Bezier = [...motionTokens.ease.inOut];
const FADE_EASE: Bezier = [0.3, 0, 0.2, 1];
const SPRING = { type: "spring" as const, visualDuration: 0.3, bounce: 0 };
const ENTER_TWEEN = { duration: motionTokens.duration.fast, ease: ENTER };
const EXIT_TWEEN = { duration: 0.1, ease: EXIT_EASE };
const cx = (...values: Array<string | false | null | undefined>) => values.filter(Boolean).join(" ");

/** Live cards can still take a decision; a sending, done or auto card cannot. */
const isLive = (state: CardState | undefined) => !state || state.phase === "idle" || state.phase === "error";

const CLOCK = new Intl.DateTimeFormat("en-GB", { hour: "2-digit", minute: "2-digit" });

function clock(iso: string) {
  const parsed = Date.parse(iso);
  return Number.isNaN(parsed) ? "" : CLOCK.format(parsed);
}

function ago(iso: string, now: number | null) {
  if (now === null) return "";
  const parsed = Date.parse(iso);
  if (Number.isNaN(parsed)) return "";
  const minutes = Math.floor((now - parsed) / 60_000);
  if (minutes < 1) return "Just now";
  if (minutes < 60) return `${minutes} min ago`;
  return clock(iso);
}

const RISK_LABEL: Record<ApprovalsRisk, string> = { low: "Low", medium: "Medium", high: "High" };
const RISK_ICON: Record<ApprovalsRisk, LucideIcon> = { low: ShieldCheck, medium: ShieldAlert, high: ShieldX };

interface MarkProps extends Omit<SVGProps<SVGSVGElement>, "title"> {
  title?: string;
  /** Brand hook the sheet keys its mark sizing off; agent marks only set it for Claude. */
  "data-brand"?: string;
}

function ClaudeMark({ title = "Claude", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="m4.7144 15.9555 4.7174-2.6471.079-.2307-.079-.1275h-.2307l-.7893-.0486-2.6956-.0729-2.3375-.0971-2.2646-.1214-.5707-.1215-.5343-.7042.0546-.3522.4797-.3218.686.0608 1.5179.1032 2.2767.1578 1.6514.0972 2.4468.255h.3886l.0546-.1579-.1336-.0971-.1032-.0972L6.973 9.8356l-2.55-1.6879-1.3356-.9714-.7225-.4918-.3643-.4614-.1578-1.0078.6557-.7225.8803.0607.2246.0607.8925.686 1.9064 1.4754 2.4893 1.8336.3643.3035.1457-.1032.0182-.0728-.164-.2733-1.3539-2.4467-1.445-2.4893-.6435-1.032-.17-.6194c-.0607-.255-.1032-.4674-.1032-.7285L6.287.1335 6.6997 0l.9957.1336.419.3642.6192 1.4147 1.0018 2.2282 1.5543 3.0296.4553.8985.2429.8318.091.255h.1579v-.1457l.1275-1.706.2368-2.0947.2307-2.6957.0789-.7589.3764-.9107.7468-.4918.5828.2793.4797.686-.0668.4433-.2853 1.8517-.5586 2.9021-.3643 1.9429h.2125l.2429-.2429.9835-1.3053 1.6514-2.0643.7286-.8196.85-.9046.5464-.4311h1.0321l.759 1.1293-.34 1.1657-1.0625 1.3478-.8804 1.1414-1.2628 1.7-.7893 1.36.0729.1093.1882-.0183 2.8535-.607 1.5421-.2794 1.8396-.3157.8318.3886.091.3946-.3278.8075-1.967.4857-2.3072.4614-3.4364.8136-.0425.0304.0486.0607 1.5482.1457.6618.0364h1.621l3.0175.2247.7892.522.4736.6376-.079.4857-1.2142.6193-1.6393-.3886-3.825-.9107-1.3113-.3279h-.1822v.1093l1.0929 1.0686 2.0035 1.8092 2.5075 2.3314.1275.5768-.3218.4554-.34-.0486-2.2039-1.6575-.85-.7468-1.9246-1.621h-.1275v.17l.4432.6496 2.3436 3.5214.1214 1.0807-.17.3521-.6071.2125-.6679-.1214-1.3721-1.9246L14.38 17.959l-1.1414-1.9428-.1397.079-.674 7.2552-.3156.3703-.7286.2793-.6071-.4614-.3218-.7468.3218-1.4753.3886-1.9246.3157-1.53.2853-1.9004.17-.6314-.0121-.0425-.1397.0182-1.4328 1.9672-2.1796 2.9446-1.7243 1.8456-.4128.164-.7164-.3704.0667-.6618.4008-.5889 2.386-3.0357 1.4389-1.882.929-1.0868-.0062-.1579h-.0546l-6.3385 4.1164-1.1293.1457-.4857-.4554.0608-.7467.2307-.2429 1.9064-1.3114Z" />
    </svg>
  );
}

function OpenAIMark({ title = "OpenAI", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="M22.2819 9.8211a5.9847 5.9847 0 0 0-.5157-4.9108 6.0462 6.0462 0 0 0-6.5098-2.9A6.0651 6.0651 0 0 0 4.9807 4.1818a5.9847 5.9847 0 0 0-3.9977 2.9 6.0462 6.0462 0 0 0 .7427 7.0966 5.98 5.98 0 0 0 .511 4.9107 6.051 6.051 0 0 0 6.5146 2.9001A5.9847 5.9847 0 0 0 13.2599 24a6.0557 6.0557 0 0 0 5.7718-4.2058 5.9894 5.9894 0 0 0 3.9977-2.9001 6.0557 6.0557 0 0 0-.7475-7.0729zm-9.022 12.6081a4.4755 4.4755 0 0 1-2.8764-1.0408l.1419-.0804 4.7783-2.7582a.7948.7948 0 0 0 .3927-.6813v-6.7369l2.02 1.1686a.071.071 0 0 1 .038.052v5.5826a4.504 4.504 0 0 1-4.4945 4.4944zm-9.6607-4.1254a4.4708 4.4708 0 0 1-.5346-3.0137l.142.0852 4.783 2.7582a.7712.7712 0 0 0 .7806 0l5.8428-3.3685v2.3324a.0804.0804 0 0 1-.0332.0615L9.74 19.9502a4.4992 4.4992 0 0 1-6.1408-1.6464zM2.3408 7.8956a4.485 4.485 0 0 1 2.3655-1.9728V11.6a.7664.7664 0 0 0 .3879.6765l5.8144 3.3543-2.0201 1.1685a.0757.0757 0 0 1-.071 0l-4.8303-2.7865A4.504 4.504 0 0 1 2.3408 7.872zm16.5963 3.8558L13.1038 8.364 15.1192 7.2a.0757.0757 0 0 1 .071 0l4.8303 2.7913a4.4944 4.4944 0 0 1-.6765 8.1042v-5.6772a.79.79 0 0 0-.407-.667zm2.0107-3.0231l-.142-.0852-4.7735-2.7818a.7759.7759 0 0 0-.7854 0L9.409 9.2297V6.8974a.0662.0662 0 0 1 .0284-.0615l4.8303-2.7866a4.4992 4.4992 0 0 1 6.6802 4.66zM8.3065 12.863l-2.02-1.1638a.0804.0804 0 0 1-.038-.0567V6.0742a4.4992 4.4992 0 0 1 7.3757-3.4537l-.142.0805L8.704 5.459a.7948.7948 0 0 0-.3927.6813zm1.0976-2.3654l2.602-1.4998 2.6069 1.4998v2.9994l-2.5974 1.4997-2.6067-1.4997Z" />
    </svg>
  );
}

function VercelMark({ title = "Vercel", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="m12 1.608 12 20.784H0Z" />
    </svg>
  );
}

function GmailMark({ title = "Gmail", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="M24 5.457v13.909c0 .904-.732 1.636-1.636 1.636h-3.819V11.73L12 16.64l-6.545-4.91v9.273H1.636A1.636 1.636 0 0 1 0 19.366V5.457c0-2.023 2.309-3.178 3.927-1.964L5.455 4.64 12 9.548l6.545-4.91 1.528-1.145C21.69 2.28 24 3.434 24 5.457z" />
    </svg>
  );
}

function PostgresqlMark({ title = "PostgreSQL", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="M23.5594 14.7228a.5269.5269 0 0 0-.0563-.1191c-.139-.2632-.4768-.3418-1.0074-.2321-1.6533.3411-2.2935.1312-2.5256-.0191 1.342-2.0482 2.445-4.522 3.0411-6.8297.2714-1.0507.7982-3.5237.1222-4.7316a1.5641 1.5641 0 0 0-.1509-.235C21.6931.9086 19.8007.0248 17.5099.0005c-1.4947-.0158-2.7705.3461-3.1161.4794a9.449 9.449 0 0 0-.5159-.0816 8.044 8.044 0 0 0-1.3114-.1278c-1.1822-.0184-2.2038.2642-3.0498.8406-.8573-.3211-4.7888-1.645-7.2219.0788C.9359 2.1526.3086 3.8733.4302 6.3043c.0409.818.5069 3.334 1.2423 5.7436.4598 1.5065.9387 2.7019 1.4334 3.582.553.9942 1.1259 1.5933 1.7143 1.7895.4474.1491 1.1327.1441 1.8581-.7279.8012-.9635 1.5903-1.8258 1.9446-2.2069.4351.2355.9064.3625 1.39.3772a.0569.0569 0 0 0 .0004.0041 11.0312 11.0312 0 0 0-.2472.3054c-.3389.4302-.4094.5197-1.5002.7443-.3102.064-1.1344.2339-1.1464.8115-.0025.1224.0329.2309.0919.3268.2269.4231.9216.6097 1.015.6331 1.3345.3335 2.5044.092 3.3714-.6787-.017 2.231.0775 4.4174.3454 5.0874.2212.5529.7618 1.9045 2.4692 1.9043.2505 0 .5263-.0291.8296-.0941 1.7819-.3821 2.5557-1.1696 2.855-2.9059.1503-.8707.4016-2.8753.5388-4.1012.0169-.0703.0357-.1207.057-.1362.0007-.0005.0697-.0471.4272.0307a.3673.3673 0 0 0 .0443.0068l.2539.0223.0149.001c.8468.0384 1.9114-.1426 2.5312-.4308.6438-.2988 1.8057-1.0323 1.5951-1.6698zM2.371 11.8765c-.7435-2.4358-1.1779-4.8851-1.2123-5.5719-.1086-2.1714.4171-3.6829 1.5623-4.4927 1.8367-1.2986 4.8398-.5408 6.108-.13-.0032.0032-.0066.0061-.0098.0094-2.0238 2.044-1.9758 5.536-1.9708 5.7495-.0002.0823.0066.1989.0162.3593.0348.5873.0996 1.6804-.0735 2.9184-.1609 1.1504.1937 2.2764.9728 3.0892.0806.0841.1648.1631.2518.2374-.3468.3714-1.1004 1.1926-1.9025 2.1576-.5677.6825-.9597.5517-1.0886.5087-.3919-.1307-.813-.5871-1.2381-1.3223-.4796-.839-.9635-2.0317-1.4155-3.5126zm6.0072 5.0871c-.1711-.0428-.3271-.1132-.4322-.1772.0889-.0394.2374-.0902.4833-.1409 1.2833-.2641 1.4815-.4506 1.9143-1.0002.0992-.126.2116-.2687.3673-.4426a.3549.3549 0 0 0 .0737-.1298c.1708-.1513.2724-.1099.4369-.0417.156.0646.3078.26.3695.4752.0291.1016.0619.2945-.0452.4444-.9043 1.2658-2.2216 1.2494-3.1676 1.0128zm2.094-3.988-.0525.141c-.133.3566-.2567.6881-.3334 1.003-.6674-.0021-1.3168-.2872-1.8105-.8024-.6279-.6551-.9131-1.5664-.7825-2.5004.1828-1.3079.1153-2.4468.079-3.0586-.005-.0857-.0095-.1607-.0122-.2199.2957-.2621 1.6659-.9962 2.6429-.7724.4459.1022.7176.4057.8305.928.5846 2.7038.0774 3.8307-.3302 4.7363-.084.1866-.1633.3629-.2311.5454zm7.3637 4.5725c-.0169.1768-.0358.376-.0618.5959l-.146.4383a.3547.3547 0 0 0-.0182.1077c-.0059.4747-.054.6489-.115.8693-.0634.2292-.1353.4891-.1794 1.0575-.11 1.4143-.8782 2.2267-2.4172 2.5565-1.5155.3251-1.7843-.4968-2.0212-1.2217a6.5824 6.5824 0 0 0-.0769-.2266c-.2154-.5858-.1911-1.4119-.1574-2.5551.0165-.5612-.0249-1.9013-.3302-2.6462.0044-.2932.0106-.5909.019-.8918a.3529.3529 0 0 0-.0153-.1126 1.4927 1.4927 0 0 0-.0439-.208c-.1226-.4283-.4213-.7866-.7797-.9351-.1424-.059-.4038-.1672-.7178-.0869.067-.276.1831-.5875.309-.9249l.0529-.142c.0595-.16.134-.3257.213-.5012.4265-.9476 1.0106-2.2453.3766-5.1772-.2374-1.0981-1.0304-1.6343-2.2324-1.5098-.7207.0746-1.3799.3654-1.7088.5321a5.6716 5.6716 0 0 0-.1958.1041c.0918-1.1064.4386-3.1741 1.7357-4.4823a4.0306 4.0306 0 0 1 .3033-.276.3532.3532 0 0 0 .1447-.0644c.7524-.5706 1.6945-.8506 2.802-.8325.4091.0067.8017.0339 1.1742.081 1.939.3544 3.2439 1.4468 4.0359 2.3827.8143.9623 1.2552 1.9315 1.4312 2.4543-1.3232-.1346-2.2234.1268-2.6797.779-.9926 1.4189.543 4.1729 1.2811 5.4964.1353.2426.2522.4522.2889.5413.2403.5825.5515.9713.7787 1.2552.0696.087.1372.1714.1885.245-.4008.1155-1.1208.3825-1.0552 1.717-.0123.1563-.0423.4469-.0834.8148-.0461.2077-.0702.4603-.0994.7662zm.8905-1.6211c-.0405-.8316.2691-.9185.5967-1.0105a2.8566 2.8566 0 0 0 .135-.0406 1.202 1.202 0 0 0 .1342.103c.5703.3765 1.5823.4213 3.0068.1344-.2016.1769-.5189.3994-.9533.6011-.4098.1903-1.0957.333-1.7473.3636-.7197.0336-1.0859-.0807-1.1721-.151zm.5695-9.2712c-.0059.3508-.0542.6692-.1054 1.0017-.055.3576-.112.7274-.1264 1.1762-.0142.4368.0404.8909.0932 1.3301.1066.887.216 1.8003-.2075 2.7014a3.5272 3.5272 0 0 1-.1876-.3856c-.0527-.1276-.1669-.3326-.3251-.6162-.6156-1.1041-2.0574-3.6896-1.3193-4.7446.3795-.5427 1.3408-.5661 2.1781-.463zm.2284 7.0137a12.3762 12.3762 0 0 0-.0853-.1074l-.0355-.0444c.7262-1.1995.5842-2.3862.4578-3.4385-.0519-.4318-.1009-.8396-.0885-1.2226.0129-.4061.0666-.7543.1185-1.0911.0639-.415.1288-.8443.1109-1.3505.0134-.0531.0188-.1158.0118-.1902-.0457-.4855-.5999-1.938-1.7294-3.253-.6076-.7073-1.4896-1.4972-2.6889-2.0395.5251-.1066 1.2328-.2035 2.0244-.1859 2.0515.0456 3.6746.8135 4.8242 2.2824a.908.908 0 0 1 .0667.1002c.7231 1.3556-.2762 6.2751-2.9867 10.5405zm-8.8166-6.1162c-.025.1794-.3089.4225-.6211.4225a.5821.5821 0 0 1-.0809-.0056c-.1873-.026-.3765-.144-.5059-.3156-.0458-.0605-.1203-.178-.1055-.2844.0055-.0401.0261-.0985.0925-.1488.1182-.0894.3518-.1226.6096-.0867.3163.0441.6426.1938.6113.4186zm7.9305-.4114c.0111.0792-.049.201-.1531.3102-.0683.0717-.212.1961-.4079.2232a.5456.5456 0 0 1-.075.0052c-.2935 0-.5414-.2344-.5607-.3717-.024-.1765.2641-.3106.5611-.352.297-.0414.6111.0088.6356.1851z" />
    </svg>
  );
}

function LinearMark({ title = "Linear", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="M2.886 4.18A11.982 11.982 0 0 1 11.99 0C18.624 0 24 5.376 24 12.009c0 3.64-1.62 6.903-4.18 9.105L2.887 4.18ZM1.817 5.626l16.556 16.556c-.524.33-1.075.62-1.65.866L.951 7.277c.247-.575.537-1.126.866-1.65ZM.322 9.163l14.515 14.515c-.71.172-1.443.282-2.195.322L0 11.358a12 12 0 0 1 .322-2.195Zm-.17 4.862 9.823 9.824a12.02 12.02 0 0 1-9.824-9.824Z" />
    </svg>
  );
}

function GithubMark({ title = "GitHub", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="M12 .297c-6.63 0-12 5.373-12 12 0 5.303 3.438 9.8 8.205 11.385.6.113.82-.258.82-.577 0-.285-.01-1.04-.015-2.04-3.338.724-4.042-1.61-4.042-1.61C4.422 18.07 3.633 17.7 3.633 17.7c-1.087-.744.084-.729.084-.729 1.205.084 1.838 1.236 1.838 1.236 1.07 1.835 2.809 1.305 3.495.998.108-.776.417-1.305.76-1.605-2.665-.3-5.466-1.332-5.466-5.93 0-1.31.465-2.38 1.235-3.22-.135-.303-.54-1.523.105-3.176 0 0 1.005-.322 3.3 1.23.96-.267 1.98-.399 3-.405 1.02.006 2.04.138 3 .405 2.28-1.552 3.285-1.23 3.285-1.23.645 1.653.24 2.873.12 3.176.765.84 1.23 1.91 1.23 3.22 0 4.61-2.805 5.625-5.475 5.92.42.36.81 1.096.81 2.22 0 1.606-.015 2.896-.015 3.286 0 .315.21.69.825.57C20.565 22.092 24 17.592 24 12.297c0-6.627-5.373-12-12-12" />
    </svg>
  );
}

function SentryMark({ title = "Sentry", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="M13.91 2.505c-.873-1.448-2.972-1.448-3.844 0L6.904 7.92a15.478 15.478 0 0 1 8.53 12.811h-2.221A13.301 13.301 0 0 0 5.784 9.814l-2.926 5.06a7.65 7.65 0 0 1 4.435 5.848H2.194a.365.365 0 0 1-.298-.534l1.413-2.402a5.16 5.16 0 0 0-1.614-.913L.296 19.275a2.182 2.182 0 0 0 .812 2.999 2.24 2.24 0 0 0 1.086.288h6.983a9.322 9.322 0 0 0-3.845-8.318l1.11-1.922a11.47 11.47 0 0 1 4.95 10.24h5.915a17.242 17.242 0 0 0-7.885-15.28l2.244-3.845a.37.37 0 0 1 .504-.13c.255.14 9.75 16.708 9.928 16.9a.365.365 0 0 1-.327.543h-2.287c.029.612.029 1.223 0 1.831h2.297a2.206 2.206 0 0 0 1.922-3.31z" />
    </svg>
  );
}

function StripeMark({ title = "Stripe", ...rest }: MarkProps) {
  return (
    <svg viewBox="0 0 24 24" fill="currentColor" role="img" aria-label={title} {...rest}>
      <path d="M13.976 9.15c-2.172-.806-3.356-1.426-3.356-2.409 0-.831.683-1.305 1.901-1.305 2.227 0 4.515.858 6.09 1.631l.89-5.494C18.252.975 15.697 0 12.165 0 9.667 0 7.589.654 6.104 1.872 4.56 3.147 3.757 4.992 3.757 7.218c0 4.039 2.467 5.76 6.476 7.219 2.585.92 3.445 1.574 3.445 2.583 0 .98-.84 1.545-2.354 1.545-1.875 0-4.965-.921-6.99-2.109l-.9 5.555C5.175 22.99 8.385 24 11.714 24c2.641 0 4.843-.624 6.328-1.813 1.664-1.305 2.525-3.236 2.525-5.732 0-4.128-2.524-5.851-6.594-7.305h.003z" />
    </svg>
  );
}

/** Gemini is drawn three times over the same glyph, each pass tinted by one brand gradient. */
function GeminiMark({ title = "Gemini", ...rest }: MarkProps) {
  const id = useId().replace(/[^a-zA-Z0-9-_]/g, "");
  const glyph = "M20.616 10.835a14.147 14.147 0 01-4.45-3.001 14.111 14.111 0 01-3.678-6.452.503.503 0 00-.975 0 14.134 14.134 0 01-3.679 6.452 14.155 14.155 0 01-4.45 3.001c-.65.28-1.318.505-2.002.678a.502.502 0 000 .975c.684.172 1.35.397 2.002.677a14.147 14.147 0 014.45 3.001 14.112 14.112 0 013.679 6.453.502.502 0 00.975 0c.172-.685.397-1.351.677-2.003a14.145 14.145 0 013.001-4.45 14.113 14.113 0 016.453-3.678.503.503 0 000-.975 13.245 13.245 0 01-2.003-.678z";
  return (
    <svg viewBox="0 0 24 24" role="img" aria-label={title} {...rest}>
      <path d={glyph} fill="#3186FF" />
      <path d={glyph} fill={`url(#${id}-g0)`} />
      <path d={glyph} fill={`url(#${id}-g1)`} />
      <path d={glyph} fill={`url(#${id}-g2)`} />
      <defs>
        <linearGradient gradientUnits="userSpaceOnUse" id={`${id}-g0`} x1="7" x2="11" y1="15.5" y2="12">
          <stop stopColor="#08B962" />
          <stop offset="1" stopColor="#08B962" stopOpacity="0" />
        </linearGradient>
        <linearGradient gradientUnits="userSpaceOnUse" id={`${id}-g1`} x1="8" x2="11.5" y1="5.5" y2="11">
          <stop stopColor="#F94543" />
          <stop offset="1" stopColor="#F94543" stopOpacity="0" />
        </linearGradient>
        <linearGradient gradientUnits="userSpaceOnUse" id={`${id}-g2`} x1="3.5" x2="17.5" y1="13.5" y2="12">
          <stop stopColor="#FABC12" />
          <stop offset=".46" stopColor="#FABC12" stopOpacity="0" />
        </linearGradient>
      </defs>
    </svg>
  );
}

/** The workspace mark: two folded faces of one sheet, lit from opposite sides. */
function NorthwindMark({ title = "Northwind", ...rest }: MarkProps) {
  const id = useId().replace(/:/g, "");
  return (
    <svg viewBox="0 0 24 24" width={16} height={16} role="img" aria-label={title} {...rest}>
      <defs>
        <linearGradient id={`${id}-light`} x1="8" y1="3" x2="8" y2="21" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="currentColor" stopOpacity=".42" />
          <stop offset="1" stopColor="currentColor" stopOpacity=".72" />
        </linearGradient>
        <linearGradient id={`${id}-dark`} x1="16" y1="3" x2="16" y2="21" gradientUnits="userSpaceOnUse">
          <stop offset="0" stopColor="currentColor" stopOpacity=".88" />
          <stop offset="1" stopColor="currentColor" stopOpacity="1" />
        </linearGradient>
      </defs>
      <path d="M12 2.5 4.75 20.75 12 16.9Z" fill={`url(#${id}-light)`} />
      <path d="M12 2.5 19.25 20.75 12 16.9Z" fill={`url(#${id}-dark)`} />
    </svg>
  );
}


const TOOL_MARKS: Record<ToolBrand, (props: MarkProps) => ReactElement> = {
  vercel: VercelMark,
  gmail: GmailMark,
  postgresql: PostgresqlMark,
  linear: LinearMark,
  github: GithubMark,
  sentry: SentryMark,
  stripe: StripeMark,
};

/** The agent's provider decides the mark, so a new agent needs no extra markup. */
function AgentMark({ agent, className }: { agent: ApprovalsAgent; className?: string }) {
  const shared: MarkProps = { className: cx(styles.mark, className), "aria-hidden": true };
  if (agent.provider === "anthropic") return <ClaudeMark {...shared} data-brand="claude" />;
  if (agent.provider === "openai") return <OpenAIMark {...shared} />;
  if (agent.provider === "google") return <GeminiMark {...shared} />;
  return <Bot {...shared} strokeWidth={1.75} />;
}

/** Tool marks fall back to a wrench, so an unknown integration still reads as a tool. */
function ToolMark({ tool, className }: { tool: ApprovalsTool; className?: string }) {
  const shared: MarkProps = { className: cx(styles.mark, className), "aria-hidden": true };
  const Mark = tool.brand ? TOOL_MARKS[tool.brand] : null;
  if (Mark) return <Mark {...shared} data-brand={tool.brand} />;
  return <Wrench {...shared} strokeWidth={1.75} />;
}

interface KbdProps {
  keys: string;
  size?: "sm";
  pressed?: boolean;
  className?: string;
  "aria-hidden"?: boolean;
}

interface KeyFace {
  glyph?: string;
  name: string;
}

const KEY_FACES: Record<string, KeyFace> = {
  enter: { glyph: "↵", name: "Return" },
  escape: { glyph: "Esc", name: "Escape" },
  esc: { glyph: "Esc", name: "Escape" },
  tab: { glyph: "⇥", name: "Tab" },
  space: { glyph: "Space", name: "Space" },
  backspace: { glyph: "⌫", name: "Delete" },
  up: { glyph: "↑", name: "Arrow up" },
  down: { glyph: "↓", name: "Arrow down" },
  left: { glyph: "←", name: "Arrow left" },
  right: { glyph: "→", name: "Arrow right" },
};

/** A key cap pair: the glyph a Mac keyboard prints, with the key's real name kept for screen readers. */
function Kbd({ keys, size = "sm", pressed = false, className, ...rest }: KbdProps) {
  const tokens = keys.split("+").filter(Boolean);
  return (
    <kbd className={cx(styles.kbd, styles.kbdSm, className)} {...rest}>
      {tokens.map(token => {
        const face = KEY_FACES[token.toLowerCase()];
        const label = face?.glyph ?? (token.length === 1 ? token.toUpperCase() : token);
        const name = face?.name;
        return (
          <span key={token} className={styles.kbdSlot}>
            <kbd className={styles.kbdKey} data-pressed={pressed || undefined}>
              {name ? <span className={styles.kbdGlyph} aria-hidden="true">{label}</span> : label}
              {name ? <span className={styles.kbdHidden}>{name}</span> : null}
            </kbd>
          </span>
        );
      })}
    </kbd>
  );
}

/** Crossfades a status chip in place while its row keeps its height. */
function Swap({ id, children, reduced, className }: { id: string; children: ReactNode; reduced: boolean; className?: string }) {
  return (
    <span className={cx(styles.swap, className)}>
      <AnimatePresence initial={false} mode="popLayout">
        <motion.span
          key={id}
          className={styles.swapItem}
          initial={{ opacity: 0, y: reduced ? 0 : 2 }}
          animate={{ opacity: 1, y: 0, transition: reduced ? { duration: 0 } : ENTER_TWEEN }}
          exit={{ opacity: 0, transition: reduced ? { duration: 0 } : EXIT_TWEEN }}
        >
          {children}
        </motion.span>
      </AnimatePresence>
    </span>
  );
}

/** Springs to the measured height of whatever it holds, so the card never jumps when the decision row changes. */
function AutoHeight({ children, reduced, className, innerClassName }: { children: ReactNode; reduced: boolean; className?: string; innerClassName?: string }) {
  const inner = useRef<HTMLDivElement>(null);
  const [height, setHeight] = useState<number | "auto">("auto");
  useLayoutEffect(() => {
    const node = inner.current;
    if (!node) return;
    const observer = new ResizeObserver(entries => {
      const entry = entries[0];
      setHeight(entry?.borderBoxSize?.[0]?.blockSize ?? node.offsetHeight);
    });
    observer.observe(node);
    return () => observer.disconnect();
  }, []);
  return (
    <motion.div className={cx(styles.autoHeight, className)} initial={false} animate={{ height }} transition={reduced ? { duration: 0 } : SPRING}>
      <div ref={inner} className={cx(styles.autoHeightInner, innerClassName)}>{children}</div>
    </motion.div>
  );
}

function RiskBadge({ risk, long = false }: { risk: ApprovalsRisk; long?: boolean }) {
  const Icon = RISK_ICON[risk];
  return (
    <span className={styles.risk} data-risk={risk}>
      <Icon size={15} strokeWidth={1.75} aria-hidden="true" />
      <span>
        {RISK_LABEL[risk]}
        <span className={long ? styles.riskWord : styles.srOnly}> risk</span>
      </span>
    </span>
  );
}

/** Backticks in a title become inline code. */
function RichText({ text }: { text: string }) {
  return <>{text.split("`").map((part, index) => index % 2 === 1 ? <code key={index} className={styles.code}>{part}</code> : <span key={index}>{part}</span>)}</>;
}

/** The sample clock is frozen to a fixed wall time and then advanced in real time, so the demo reads the same on every reload. */
function useClock(demo: boolean) {
  const wall = useSyncExternalStore(
    useCallback((notify: () => void) => {
      const id = window.setInterval(notify, 15_000);
      return () => window.clearInterval(id);
    }, []),
    () => 15_000 * Math.floor(Date.now() / 15_000),
    () => null,
  );
  const [origin] = useState(() => ({ wall: Date.now(), demo: Date.parse("2026-10-20T14:32:00") }));
  const now = wall === null ? null : demo ? origin.demo + Math.max(0, wall - origin.wall) : wall;
  const stamp = useCallback(
    () => new Date(demo ? origin.demo + (Date.now() - origin.wall) : Date.now()).toISOString(),
    [demo, origin],
  );
  return { now, stamp };
}

interface ArgRowProps {
  arg: ApprovalArg;
  value: string;
  edited: boolean;
  disabled: boolean;
  onChange: (value: string) => void;
  onRestore: () => void;
}

/** A value a person can rewrite in place. Escape cancels, Enter commits, and the row keeps a way back to the agent's value. */
function ArgRow({ arg, value, edited, disabled, onChange, onRestore }: ArgRowProps) {
  const [editing, setEditing] = useState(false);
  const [draft, setDraft] = useState(value);
  const input = useRef<HTMLInputElement>(null);
  const textarea = useRef<HTMLTextAreaElement>(null);
  const button = useRef<HTMLButtonElement>(null);
  const restoreFocus = useRef(false);
  const id = useId();
  const editable = arg.editable !== false;

  useEffect(() => {
    if (editing) {
      const editor = arg.multiline ? textarea.current : input.current;
      editor?.focus();
      editor?.select();
      return;
    }
    if (restoreFocus.current) {
      restoreFocus.current = false;
      button.current?.focus();
    }
  }, [arg.multiline, editing]);

  const commit = (save: boolean, refocus: boolean) => {
    const next = draft.trim();
    if (save && next && next !== value) onChange(next);
    restoreFocus.current = refocus;
    setEditing(false);
  };

  const onKeyDown = (event: ReactKeyboardEvent<HTMLInputElement | HTMLTextAreaElement>) => {
    if (event.key === "Escape") {
      event.preventDefault();
      event.stopPropagation();
      commit(false, true);
      return;
    }
    if (event.key === "Enter" && (!arg.multiline || event.metaKey || event.ctrlKey)) {
      event.preventDefault();
      event.stopPropagation();
      commit(true, true);
    }
  };

  const valueClassName = cx(styles.argValue, arg.mono ? styles.mono : "");

  return (
    <div className={styles.argRow} data-edited={edited || undefined}>
      <dt className={styles.argKey}>
        {editing ? <label htmlFor={id}>{arg.label}</label> : arg.label}
        {edited && <span className={styles.srOnly}>, edited</span>}
      </dt>
      <dd className={styles.argCell}>
        {!editable || disabled ? (
          <span className={valueClassName} data-static={!editable || undefined}>{value}</span>
        ) : editing ? (
          arg.multiline ? (
            <textarea
              ref={textarea}
              id={id}
              className={cx(valueClassName, styles.field)}
              value={draft}
              rows={2}
              onChange={event => setDraft(event.target.value)}
              onKeyDown={onKeyDown}
              onBlur={() => commit(true, false)}
              aria-describedby={`${id}-hint`}
              spellCheck={false}
            />
          ) : (
            <input
              ref={input}
              id={id}
              className={cx(valueClassName, styles.field)}
              value={draft}
              onChange={event => setDraft(event.target.value)}
              onKeyDown={onKeyDown}
              onBlur={() => commit(true, false)}
              aria-describedby={`${id}-hint`}
              spellCheck={false}
              autoComplete="off"
            />
          )
        ) : (
          <button
            ref={button}
            type="button"
            className={cx(valueClassName, styles.valueButton)}
            onClick={() => { setDraft(value); setEditing(true); }}
            aria-label={`${arg.label}: ${value}. Edit`}
          >
            <span className={styles.valueText}>{value}</span>
            <Pencil className={styles.editIcon} size={13} strokeWidth={1.75} aria-hidden="true" />
          </button>
        )}
        {editing && (
          <span id={`${id}-hint`} className={styles.srOnly}>
            {arg.multiline ? "Command Enter saves, Escape cancels" : "Enter saves, Escape cancels"}
          </span>
        )}
      </dd>
      {edited && !editing && !disabled && (
        <button type="button" className={styles.restore} onClick={onRestore} aria-label={`Restore ${arg.label} to the agent's value`} title="Restore the agent's value">
          <RotateCcw size={13} strokeWidth={1.75} aria-hidden="true" />
        </button>
      )}
    </div>
  );
}

interface DecisionProps {
  request: ApprovalRequest;
  state: CardState;
  reduced: boolean;
  workspace: string;
  workspaceMark?: ReactNode;
  demo: boolean;
  allowRef: React.RefObject<HTMLSpanElement | null>;
  onDecide: (outcome: DecideOutcome) => void;
  onAlwaysAllow: (scope: "agent" | "workspace") => void;
}

/** The one row under the arguments: ask, busy, or already settled. */
function Decision({ request, state, reduced, workspace, workspaceMark, demo, allowRef, onDecide, onAlwaysAllow }: DecisionProps) {
  const agent = request.agent;
  switch (state.phase) {
    case "sending":
      return (
        <p className={styles.line}>
          <TextShimmer active={!reduced}>
            {state.outcome === "denied" ? `Telling ${agent.name} no` : `Sending your approval to ${agent.name}`}
          </TextShimmer>
        </p>
      );
    case "done":
      if (state.rule) {
        return (
          <p className={styles.line}>
            <ShieldCheck className={styles.lineIcon} data-tone="accent" size={16} strokeWidth={1.75} aria-hidden="true" />
            <span>
              Approved
              <span className={styles.lineNote}>
                {state.rule.scope === "agent" ? `Always allowed for ${agent.name}` : `Always allowed for any agent in ${workspace}`}
              </span>
            </span>
          </p>
        );
      }
      if (state.outcome === "denied") {
        return (
          <p className={styles.line}>
            <X className={styles.lineIcon} data-tone="muted" size={16} strokeWidth={1.75} aria-hidden="true" />
            <span>
              Denied
              <span className={styles.lineNote}>{agent.name} will not run it</span>
            </span>
          </p>
        );
      }
      return (
        <p className={styles.line}>
          <Check className={styles.lineIcon} data-tone="success" size={16} strokeWidth={1.75} aria-hidden="true" />
          <span>
            {state.edited ? "Approved with your edits" : "Approved"}
            <span className={styles.lineNote}>{demo ? "Simulated, nothing ran" : `${agent.name} can go ahead`}</span>
          </span>
        </p>
      );
    case "error":
      return (
        <div className={styles.errorRow} role="alert">
          <p className={styles.errorText}>
            <CircleAlert className={styles.lineIcon} data-tone="danger" size={16} strokeWidth={1.75} aria-hidden="true" />
            {state.message ?? "Your decision was not sent. Nothing ran."}
          </p>
          <span className={styles.actionsEnd}>
            <Button variant="ghost" size="sm" onClick={() => onDecide("denied")}>Deny</Button>
            <Button variant="secondary" size="sm" onClick={() => onDecide(state.outcome === "denied" ? "denied" : "approved")}>
              <RotateCcw size={15} strokeWidth={1.75} aria-hidden="true" />
              Retry
            </Button>
          </span>
        </div>
      );
    default:
      return (
        <div className={styles.actions}>
          <Button variant="ghost" size="sm" className={styles.deny} onClick={() => onDecide("denied")} aria-keyshortcuts="d">Deny</Button>
          <span className={styles.actionsEnd}>
            <span ref={allowRef} className={styles.allow}>
              <DropdownMenu
                label="Always allow"
                items={[
                  { label: `For ${agent.name}`, icon: <User size={16} strokeWidth={1.75} />, onSelect: () => onAlwaysAllow("agent") },
                  { label: `For any agent in ${workspace}`, icon: workspaceMark ?? <Building2 size={16} strokeWidth={1.75} />, onSelect: () => onAlwaysAllow("workspace") },
                ]}
              />
            </span>
            <Button size="sm" className={styles.approve} onClick={() => onDecide("approved")} aria-keyshortcuts="Enter a">
              Approve
              <Kbd keys="Enter" size="sm" className={styles.approveKbd} aria-hidden />
            </Button>
          </span>
        </div>
      );
  }
}

interface FocusCardProps {
  request: ApprovalRequest;
  state: CardState;
  edits: Record<string, string>;
  now: number | null;
  reduced: boolean;
  workspace: string;
  workspaceMark?: ReactNode;
  demo: boolean;
  allowRef: React.RefObject<HTMLSpanElement | null>;
  onEdit: (key: string, value: string) => void;
  onRestore: (key: string) => void;
  onDecide: (outcome: DecideOutcome) => void;
  onAlwaysAllow: (scope: "agent" | "workspace") => void;
}

function FocusCard({ request, state, edits, now, reduced, workspace, workspaceMark, demo, allowRef, onEdit, onRestore, onDecide, onAlwaysAllow }: FocusCardProps) {
  const titleId = useId();
  const live = isLive(state);
  const decisionKey = state.phase === "idle" ? "idle" : `${state.phase}-${state.outcome ?? ""}-${state.phase === "done" && state.rule ? "rule" : ""}`;
  return (
    <div className={styles.focus} role="group" aria-labelledby={titleId}>
      <div className={styles.focusMeta}>
        <span className={styles.agent}>
          <AgentMark agent={request.agent} className={styles.agentMark} />
          <span className={styles.agentName}>{request.agent.name}</span>
          <span className={styles.agentModel}>{request.agent.model}</span>
          <time className={styles.agentTime} dateTime={request.requestedAt} title={`Asked at ${clock(request.requestedAt)}`} suppressHydrationWarning>
            {ago(request.requestedAt, now)}
          </time>
        </span>
        <RiskBadge risk={request.risk} long />
      </div>
      <h3 id={titleId} className={styles.focusTitle}>
        <ToolMark tool={request.tool} className={styles.titleMark} />
        <span className={styles.titleText}>
          <RichText text={request.title} />
          <span className={styles.srOnly}>{`, asked by ${request.agent.name}`}</span>
        </span>
      </h3>
      <p className={styles.reason}>{request.reason}</p>
      <dl className={styles.args} aria-label="Arguments">
        {request.args.map(arg => (
          <ArgRow
            key={arg.key}
            arg={arg}
            value={edits[arg.key] ?? arg.value}
            edited={(edits[arg.key] ?? arg.value) !== arg.value}
            disabled={!live}
            onChange={value => onEdit(arg.key, value)}
            onRestore={() => onRestore(arg.key)}
          />
        ))}
      </dl>
      <AutoHeight reduced={reduced} className={styles.decisionSlot}>
        <div className={styles.decisionStack}>
          <AnimatePresence initial={false} mode="popLayout">
            <motion.div
              key={decisionKey}
              className={styles.decision}
              initial={{ opacity: 0, y: reduced ? 0 : 2 }}
              animate={{ opacity: 1, y: 0, transition: reduced ? { duration: 0 } : ENTER_TWEEN }}
              exit={{ opacity: 0, transition: reduced ? { duration: 0 } : EXIT_TWEEN }}
            >
              <Decision
                request={request}
                state={state}
                reduced={reduced}
                workspace={workspace}
                workspaceMark={workspaceMark}
                demo={demo}
                allowRef={allowRef}
                onDecide={onDecide}
                onAlwaysAllow={onAlwaysAllow}
              />
            </motion.div>
          </AnimatePresence>
        </div>
      </AutoHeight>
    </div>
  );
}

interface RulesButtonProps {
  rules: ApprovalRule[];
  workspace: string;
  reduced: boolean;
  bump: number;
  open: boolean;
  onOpenChange: (open: boolean) => void;
  onRemove: (rule: ApprovalRule) => void;
}

/** Rules live behind a small counter button, so the panel stays out of the way until it is asked for. */
const RulesButton = forwardRef<HTMLButtonElement, RulesButtonProps>(function RulesButton({ rules, workspace, reduced, bump, open, onOpenChange, onRemove }, ref) {
  const list = useRef<HTMLUListElement>(null);
  return (
    <Popover open={open} onOpenChange={onOpenChange}>
      <PopoverTrigger ref={ref} className={styles.rulesButton} aria-label={`Rules, ${rules.length}`}>
        {bump > 0 && <span key={bump} className={styles.rulesFlash} data-reduced={reduced || undefined} aria-hidden="true" />}
        <span className={styles.rulesLabel} aria-hidden="true">
          Rules
          <span className={styles.dot}>·</span>
          <AnimatedCounter value={rules.length} />
        </span>
      </PopoverTrigger>
      <PopoverContent align="end" className={styles.rulesPanel} data-rules-panel="" tabIndex={-1} aria-label="Always allow rules">
        <p className={styles.panelTitle}>Always allowed</p>
        <AutoHeight reduced={reduced}>
          {rules.length === 0 && (
            <p className={styles.panelEmpty}>No rules yet. Choose Always allow on a request to skip the ask next time.</p>
          )}
          <ul ref={list} className={styles.ruleList}>
            <AnimatePresence initial={false}>
              {rules.map((rule, index) => (
                <motion.li
                  key={rule.id}
                  className={styles.ruleItem}
                  initial={reduced ? { opacity: 0 } : { opacity: 0, height: 0 }}
                  animate={{ opacity: 1, height: "auto" }}
                  exit={reduced ? { opacity: 0 } : { opacity: 0, height: 0 }}
                  transition={reduced ? { duration: 0 } : { height: SPRING, opacity: { duration: 0.16 } }}
                >
                  <div className={styles.rule}>
                    <ToolMark tool={rule.tool} className={styles.ruleMark} />
                    <span className={styles.ruleText}>
                      <span className={styles.ruleAction}>{rule.tool.action}</span>
                      <span className={styles.ruleScope} title={rule.createdBy ? `Added by ${rule.createdBy}` : undefined}>
                        {rule.scope === "agent" ? `${rule.agent?.name ?? "One agent"} only` : `Any agent in ${workspace}`}
                      </span>
                    </span>
                    <button
                      type="button"
                      className={styles.iconButton}
                      data-remove=""
                      onClick={() => {
                        const buttons = Array.from(list.current?.querySelectorAll<HTMLButtonElement>("[data-remove]") ?? []);
                        const hadFocus = buttons[index] === document.activeElement;
                        onRemove(rule);
                        if (!hadFocus) return;
                        // The row is about to leave, so focus walks to the next rule, the previous one, or the panel itself.
                        const next = buttons[index + 1] ?? buttons[index - 1];
                        if (next) next.focus();
                        else list.current?.closest<HTMLElement>("[data-rules-panel]")?.focus();
                      }}
                      aria-label={`Remove rule: ${rule.tool.action}, ${rule.scope === "agent" ? (rule.agent?.name ?? "one agent") : "any agent"}`}
                    >
                      <X size={15} strokeWidth={1.75} aria-hidden="true" />
                    </button>
                  </div>
                </motion.li>
              ))}
            </AnimatePresence>
          </ul>
        </AutoHeight>
      </PopoverContent>
    </Popover>
  );
});

const OUTCOME_LABEL: Record<ResolvedEntry["outcome"], string> = { approved: "Approved", denied: "Denied", allowed: "Allowed by rule" };

/** Each outcome keeps its own mark: a check for approval, an X for denial, a shield once a rule covered it. */
function OutcomeIcon({ outcome }: { outcome: ResolvedEntry["outcome"] }) {
  if (outcome === "denied") return <X size={14} strokeWidth={1.75} aria-hidden="true" />;
  if (outcome === "allowed") return <ShieldCheck size={14} strokeWidth={1.75} aria-hidden="true" />;
  return <Check size={14} strokeWidth={1.75} aria-hidden="true" />;
}

/** The audit trail: newest first, one line per decision, so the reviewer can check what already ran. */
function ResolvedLog({ entries, reduced, id }: { entries: ResolvedEntry[]; reduced: boolean; id: string }) {
  return (
    <ul id={id} className={styles.log} aria-label="Resolved requests">
      <AnimatePresence initial={false}>
        {entries.map(entry => {
          return (
            <motion.li
              key={entry.id}
              className={styles.logItem}
              initial={reduced ? { opacity: 0 } : { opacity: 0, height: 0 }}
              animate={{ opacity: 1, height: "auto" }}
              transition={reduced ? { duration: 0 } : { height: SPRING, opacity: { duration: 0.16 } }}
            >
              <div className={styles.logRow}>
                <ToolMark tool={entry.tool} className={styles.logMark} />
                <span className={styles.logText}>
                  <span className={styles.logSummary} title={entry.summary}>{entry.summary}</span>
                  <span className={styles.logMeta}>
                    <span className={styles.logOutcome} data-outcome={entry.outcome}>
                      <OutcomeIcon outcome={entry.outcome} />
                      {OUTCOME_LABEL[entry.outcome]}
                      {entry.edited ? " with edits" : ""}
                    </span>
                    <span>{entry.decidedBy === "Rule" ? entry.agentName : `by ${entry.decidedBy}`}</span>
                  </span>
                </span>
                <time className={styles.logTime} dateTime={entry.decidedAt}>{clock(entry.decidedAt)}</time>
              </div>
            </motion.li>
          );
        })}
      </AnimatePresence>
    </ul>
  );
}

const at = (time: string) => `2026-10-20T${time}:00`;

const DEPLOY_AGENT: ApprovalsAgent = { id: "deploy", name: "Deploy agent", model: "Claude Opus 5.5", provider: "anthropic" };
const SUPPORT_AGENT: ApprovalsAgent = { id: "support", name: "Support agent", model: "GPT-6 Luna", provider: "openai" };
const DATA_AGENT: ApprovalsAgent = { id: "data", name: "Data agent", model: "Gemini 4 Argon", provider: "google" };
const TRIAGE_AGENT: ApprovalsAgent = { id: "triage", name: "Triage agent", model: "Claude Haiku 5.5", provider: "anthropic" };

const VERCEL_TOOL: ApprovalsTool = { id: "vercel.deploy", name: "Vercel", action: "Deploy to production", brand: "vercel" };
const GMAIL_TOOL: ApprovalsTool = { id: "gmail.send", name: "Gmail", action: "Send email", brand: "gmail" };
const POSTGRES_TOOL: ApprovalsTool = { id: "postgres.write", name: "PostgreSQL", action: "Write to billing-prod", brand: "postgresql" };
const GITHUB_TOOL: ApprovalsTool = { id: "github.pr", name: "GitHub", action: "Open pull requests", brand: "github" };
const SENTRY_TOOL: ApprovalsTool = { id: "sentry.read", name: "Sentry", action: "Read issues", brand: "sentry" };
const LINEAR_TOOL: ApprovalsTool = { id: "linear.close", name: "Linear", action: "Close issues", brand: "linear" };
const STRIPE_TOOL: ApprovalsTool = { id: "stripe.refund", name: "Stripe", action: "Issue refunds", brand: "stripe" };

const SAMPLE_REQUESTS: ApprovalRequest[] = [
  {
    id: "req-deploy-web",
    agent: DEPLOY_AGENT,
    tool: VERCEL_TOOL,
    title: "Run `vercel deploy --prod`",
    summary: "Deploy northwind-web to production",
    reason: "The preview passed all 214 checks and the release window opened at 14:00.",
    args: [
      { key: "project", label: "Project", value: "northwind-web", mono: true },
      { key: "commit", label: "Commit", value: "8f3c2a1 Ship pricing v3", mono: true, editable: false },
      { key: "target", label: "Target", value: "production", mono: true },
      { key: "regions", label: "Regions", value: "fra1, iad1", mono: true },
    ],
    risk: "high",
    requestedAt: at("14:30"),
  },
  {
    id: "req-email-dana",
    agent: SUPPORT_AGENT,
    tool: GMAIL_TOOL,
    title: "Send an email with Gmail",
    summary: "Email dana@halcyon.studio about her refund",
    reason: "Dana asked for an update twice today, and the refund cleared in Stripe at 13:52.",
    args: [
      { key: "to", label: "To", value: "dana@halcyon.studio" },
      { key: "subject", label: "Subject", value: "Your refund for order #48213" },
      { key: "body", label: "Body", value: "Hi Dana, I have refunded $129.00 to your Visa ending 4242. It usually arrives in 5 to 10 business days.", multiline: true },
    ],
    risk: "medium",
    requestedAt: at("14:27"),
  },
  {
    id: "req-sql-plans",
    agent: DATA_AGENT,
    tool: POSTGRES_TOOL,
    title: "`UPDATE` 1,284 rows in PostgreSQL",
    summary: "Move 1,284 legacy plans to team",
    reason: "Moves legacy team plans to the new plan before the 02:00 billing run.",
    args: [
      { key: "database", label: "Database", value: "billing-prod", mono: true, editable: false },
      { key: "statement", label: "Statement", value: "UPDATE subscriptions SET plan = 'team' WHERE plan = 'team_legacy';", mono: true, multiline: true },
      { key: "rows", label: "Rows", value: "1,284", editable: false },
    ],
    risk: "high",
    requestedAt: at("14:21"),
  },
  {
    id: "req-deploy-docs",
    agent: DEPLOY_AGENT,
    tool: VERCEL_TOOL,
    title: "Run `vercel deploy --prod`",
    summary: "Deploy northwind-docs to production",
    reason: "The pricing v3 docs are reviewed and should ship with the web release.",
    args: [
      { key: "project", label: "Project", value: "northwind-docs", mono: true },
      { key: "commit", label: "Commit", value: "c41d9e7 Update pricing pages", mono: true, editable: false },
      { key: "target", label: "Target", value: "production", mono: true },
      { key: "regions", label: "Regions", value: "fra1", mono: true },
    ],
    risk: "medium",
    requestedAt: at("14:19"),
  },
  {
    id: "req-close-web",
    agent: TRIAGE_AGENT,
    tool: LINEAR_TOOL,
    title: "Close 6 issues in Linear",
    summary: "Close 6 duplicates of WEB-398",
    reason: "All six report the Safari checkout bug that WEB-398 fixed in v2.14.",
    args: [
      { key: "issues", label: "Issues", value: "WEB-412, WEB-418, WEB-421, WEB-427, WEB-433, WEB-440", mono: true },
      { key: "state", label: "Close as", value: "Duplicate of WEB-398" },
      { key: "comment", label: "Comment", value: "Fixed by WEB-398, shipped in v2.14." },
    ],
    risk: "low",
    requestedAt: at("14:16"),
  },
];

const SAMPLE_RULES: ApprovalRule[] = [
  { id: "rule-github-pr", tool: GITHUB_TOOL, scope: "agent", agent: { id: DEPLOY_AGENT.id, name: DEPLOY_AGENT.name }, createdBy: "Marcus Johnson", createdAt: at("09:12") },
  { id: "rule-sentry-read", tool: SENTRY_TOOL, scope: "workspace", createdBy: "Emma Collins", createdAt: at("10:40") },
];

const SAMPLE_RESOLVED: ResolvedEntry[] = [
  { id: "log-refund", agentName: SUPPORT_AGENT.name, tool: STRIPE_TOOL, summary: "Refund $129.00 on order #48213", outcome: "approved", decidedBy: "Emma Collins", decidedAt: at("13:52") },
  { id: "log-events", agentName: DATA_AGENT.name, tool: POSTGRES_TOOL, summary: "Delete events older than January 2025", outcome: "denied", decidedBy: "Marcus Johnson", decidedAt: at("13:41") },
  { id: "log-sentry", agentName: TRIAGE_AGENT.name, tool: SENTRY_TOOL, summary: "Read 14 issues in northwind-web", outcome: "allowed", decidedBy: "Rule", decidedAt: at("13:30") },
  { id: "log-pr", agentName: DEPLOY_AGENT.name, tool: GITHUB_TOOL, summary: "Open pull request #2143", outcome: "allowed", decidedBy: "Rule", decidedAt: at("12:58") },
  { id: "log-api", agentName: DEPLOY_AGENT.name, tool: VERCEL_TOOL, summary: "Deploy northwind-api to production", outcome: "approved", edited: true, decidedBy: "Emma Collins", decidedAt: at("11:20") },
  { id: "log-invoice", agentName: SUPPORT_AGENT.name, tool: GMAIL_TOOL, summary: "Email leo@brightpath.io about invoice 1042", outcome: "denied", decidedBy: "Emma Collins", decidedAt: at("10:05") },
];

type KeyEventLike = Pick<KeyboardEvent, "defaultPrevented" | "metaKey" | "ctrlKey" | "altKey" | "key" | "target" | "preventDefault">;

interface Flight {
  id: string;
  rule: ApprovalRule;
  from: { x: number; y: number };
  to: { x: number; y: number };
}

/**
 * A queue of tool calls that need a person, one card in focus at a time. Approving, denying, or saving a rule
 * settles the card, slides the next request up, and writes the decision to the audit log. In the built in sample
 * queue the first approval of the PostgreSQL request fails on purpose, so the error path can be inspected.
 */
export function AgentApprovals({
  requests,
  rules,
  resolved,
  reviewer = "Emma Collins",
  workspace = "Northwind",
  workspaceMark,
  title = "Approvals",
  onDecision,
  onRuleChange,
  className,
}: AgentApprovalsProps) {
  const demo = requests === undefined;
  const reduced = Boolean(useReducedMotion());
  const rootId = useId();
  const { now, stamp } = useClock(demo);
  const [queue, setQueue] = useState<ApprovalRequest[]>(() => requests ?? SAMPLE_REQUESTS);
  const [focusId, setFocusId] = useState<string | null>(() => (requests ?? SAMPLE_REQUESTS)[0]?.id ?? null);
  const [cards, setCards] = useState<Record<string, CardState>>({});
  const [edits, setEdits] = useState<Record<string, Record<string, string>>>({});
  const [ruleList, setRuleList] = useState<ApprovalRule[]>(() => rules ?? (demo ? SAMPLE_RULES : []));
  const [log, setLog] = useState<ResolvedEntry[]>(() => resolved ?? (demo ? SAMPLE_RESOLVED : []));
  const [mode, setMode] = useState<"swap" | "advance">("swap");
  const [flight, setFlight] = useState<Flight | null>(null);
  const [bump, setBump] = useState(0);
  const [logOpen, setLogOpen] = useState(false);
  const [rulesOpen, setRulesOpen] = useState(false);
  const [status, setStatus] = useState("");
  const [pressedKey, setPressedKey] = useState<string | null>(null);
  const [previousRequests, setPreviousRequests] = useState(requests);

  // Requests that arrive later are appended, and the card takes focus if nothing else is waiting.
  if (requests !== previousRequests) {
    setPreviousRequests(requests);
    if (requests) {
      const settledIds = new Set(log.map(entry => entry.id));
      const queuedIds = new Set(queue.map(request => request.id));
      const added = requests.filter(request => !queuedIds.has(request.id) && !settledIds.has(request.id));
      if (added.length > 0) {
        setQueue(current => [...current, ...added]);
        setFocusId(current => current ?? added[0]?.id ?? null);
      }
    }
  }

  const root = useRef<HTMLElement>(null);
  const focusArea = useRef<HTMLDivElement>(null);
  const allowButton = useRef<HTMLSpanElement>(null);
  const rulesButton = useRef<HTMLButtonElement>(null);
  const latest = useRef({ queue, cards, edits, focusId, onDecision, onRuleChange });
  useLayoutEffect(() => {
    latest.current = { queue, cards, edits, focusId, onDecision, onRuleChange };
  });

  const timers = useRef(new Set<number>());
  const settled = useRef(new Set<string>());
  const simulatedFailureUsed = useRef(false);
  const inside = useRef(false);
  const afterFlight = useRef<(() => void) | null>(null);

  useEffect(() => {
    const pending = timers.current;
    return () => {
      pending.forEach(id => window.clearTimeout(id));
      pending.clear();
    };
  }, []);

  const schedule = useCallback((run: () => void, delay: number) => {
    const id = window.setTimeout(() => {
      timers.current.delete(id);
      run();
    }, delay);
    timers.current.add(id);
  }, []);

  const announce = useCallback((message: string) => {
    setStatus(current => current === message ? `${message} ` : message);
  }, []);

  const setCard = useCallback((id: string, next: CardState) => {
    setCards(current => ({ ...current, [id]: next }));
  }, []);

  /** After a decision the card is gone, so focus falls back to the focus region unless something else already holds it. */
  const restoreFocus = useCallback(() => {
    schedule(() => {
      const active = document.activeElement;
      if ((!active || active === document.body) && inside.current) focusArea.current?.focus({ preventScroll: true });
    }, 150);
  }, [schedule]);

  const argsFor = (request: ApprovalRequest) => {
    const stored = latest.current.edits[request.id] ?? {};
    return {
      args: Object.fromEntries(request.args.map(arg => [arg.key, stored[arg.key] ?? arg.value])),
      edited: request.args.some(arg => stored[arg.key] !== undefined && stored[arg.key] !== arg.value),
    };
  };

  const send = useCallback((decision: ApprovalDecision, shouldFail: boolean) => {
    const handler = latest.current.onDecision;
    if (handler) {
      try {
        return Promise.resolve(handler(decision)).then(() => undefined);
      } catch (error) {
        return Promise.reject(error instanceof Error ? error : new Error(String(error)));
      }
    }
    return new Promise<void>((resolve, reject) => {
      schedule(() => shouldFail ? reject(new Error("timeout")) : resolve(), decision.outcome === "denied" ? 280 : 460);
    });
  }, [schedule]);

  /** Settles a card: it leaves the queue, the next live request comes up, and the decision joins the log. */
  const settle = useCallback((request: ApprovalRequest, settlement: Settlement) => {
    const { queue: current, cards: cardMap, focusId: focused } = latest.current;
    const index = current.findIndex(item => item.id === request.id);
    if (index < 0) return;
    settled.current.delete(request.id);
    const rest = current.filter(item => item.id !== request.id);
    if (focused === request.id) {
      const next = [...rest.slice(index), ...rest.slice(0, index)].find(item => isLive(cardMap[item.id]) && !settled.current.has(item.id)) ?? null;
      setMode("advance");
      setFocusId(next?.id ?? null);
    }
    setQueue(currentQueue => currentQueue.filter(item => item.id !== request.id));
    setLog(currentLog => [{ id: request.id, agentName: request.agent.name, tool: request.tool, summary: request.summary, ...settlement }, ...currentLog]);
    restoreFocus();
  }, [restoreFocus]);

  const decide = useCallback((requestId: string, outcome: DecideOutcome) => {
    const request = latest.current.queue.find(item => item.id === requestId);
    if (!request || !isLive(latest.current.cards[requestId]) || settled.current.has(requestId)) return;
    settled.current.add(requestId);
    const { args, edited } = argsFor(request);
    const decidedAt = stamp();
    const decision: ApprovalDecision = { requestId, outcome, args, edited, decidedBy: reviewer, decidedAt };
    // One approval in the sample queue fails on purpose so the retry path can be seen.
    const fails = demo && !latest.current.onDecision && outcome === "approved" && requestId === "req-sql-plans" && !simulatedFailureUsed.current;
    if (fails) simulatedFailureUsed.current = true;
    setCard(requestId, { phase: "sending", outcome });
    restoreFocus();
    send(decision, fails).then(
      () => {
        setCard(requestId, { phase: "done", outcome, edited });
        announce(`${outcome === "approved" ? (edited ? "Approved with your edits" : "Approved") : "Denied"}: ${request.agent.name}, ${request.summary}.${demo ? " Simulated, nothing ran." : ""}`);
        schedule(() => settle(request, { outcome, edited, decidedBy: reviewer, decidedAt }), 620);
      },
      (error: unknown) => {
        const message = demo && !latest.current.onDecision
          ? `${request.agent.name} did not answer, so nothing ran. This failure is simulated.`
          : error instanceof Error && error.message
            ? error.message
            : `${request.agent.name} did not get your decision. Nothing ran.`;
        settled.current.delete(requestId);
        setCard(requestId, { phase: "error", outcome, message });
        announce(`Not sent. ${message} Retry or deny.`);
      },
    );
  }, [announce, demo, restoreFocus, reviewer, schedule, send, setCard, stamp, settle]);

  const alwaysAllow = useCallback((scope: "agent" | "workspace") => {
    const focused = latest.current.focusId;
    const request = latest.current.queue.find(item => item.id === focused);
    if (!focused || !request || !isLive(latest.current.cards[focused]) || settled.current.has(focused)) return;
    settled.current.add(focused);
    const { args, edited } = argsFor(request);
    const decidedAt = stamp();
    const rule: ApprovalRule = {
      id: `rule-${request.tool.id}-${scope === "agent" ? request.agent.id : "all"}-${Date.now().toString(36)}`,
      tool: request.tool,
      scope,
      agent: scope === "agent" ? { id: request.agent.id, name: request.agent.name } : undefined,
      createdBy: reviewer,
      createdAt: decidedAt,
    };
    setCard(focused, { phase: "done", outcome: "approved", rule, edited });
    restoreFocus();

    const commit = () => {
      afterFlight.current = null;
      setFlight(null);
      setRuleList(current => [rule, ...current]);
      setBump(current => current + 1);
      latest.current.onRuleChange?.({ type: "added", rule });
      const { queue: waiting, cards: cardMap } = latest.current;
      const covered = waiting.filter(item =>
        item.id !== focused &&
        rule.tool.id === item.tool.id &&
        (rule.scope === "workspace" || rule.agent?.id === item.agent.id) &&
        isLive(cardMap[item.id]) &&
        !settled.current.has(item.id),
      );
      covered.forEach(item => settled.current.add(item.id));
      const who = scope === "agent" ? request.agent.name : `Any agent in ${workspace}`;
      announce(`Rule added: ${who} can ${rule.tool.action.toLowerCase()} without asking.${covered.length > 0 ? ` It allowed ${covered.length} waiting ${covered.length === 1 ? "request" : "requests"}.` : ""}`);
      covered.forEach((item, index) => {
        setCard(item.id, { phase: "auto", outcome: "allowed", rule });
        const { args: coveredArgs, edited: coveredEdited } = argsFor(item);
        const coveredAt = stamp();
        send({ requestId: item.id, outcome: "allowed", args: coveredArgs, edited: coveredEdited, ruleId: rule.id, decidedBy: "Rule", decidedAt: coveredAt }, false).then(
          () => schedule(() => settle(item, { outcome: "allowed", decidedBy: "Rule", decidedAt: coveredAt }), 1100 + 80 * index),
          () => {
            settled.current.delete(item.id);
            setCard(item.id, IDLE);
            announce(`The rule matched ${item.summary}, but ${item.agent.name} did not get the approval. It is still waiting.`);
          },
        );
      });
    };

    const box = root.current?.getBoundingClientRect();
    const center = (node: Element | null) => {
      const rect = node?.getBoundingClientRect();
      return rect && box ? { x: rect.left - box.left + rect.width / 2, y: rect.top - box.top + rect.height / 2 } : null;
    };
    const from = center(allowButton.current);
    const to = center(rulesButton.current);
    if (!reduced && from && to) {
      afterFlight.current = commit;
      setFlight({ id: rule.id, rule, from, to });
    } else {
      commit();
    }

    const started = Date.now();
    send({ requestId: focused, outcome: "approved", args, edited, ruleId: rule.id, decidedBy: reviewer, decidedAt }, false).then(
      () => schedule(() => settle(request, { outcome: "approved", edited, decidedBy: reviewer, decidedAt }), Math.max(620, 920 - (Date.now() - started))),
      () => {
        settled.current.delete(focused);
        setCard(focused, { phase: "error", outcome: "approved", message: `The rule is saved, but ${request.agent.name} did not get this approval. Nothing ran.` });
      },
    );
  }, [announce, demo, reduced, restoreFocus, reviewer, schedule, send, setCard, settle, stamp, workspace]);

  const removeRule = useCallback((rule: ApprovalRule) => {
    setRuleList(current => current.filter(item => item.id !== rule.id));
    latest.current.onRuleChange?.({ type: "removed", rule });
    const who = rule.scope === "agent" ? (rule.agent?.name ?? "This agent") : `Every agent in ${workspace}`;
    announce(`Rule removed. ${who} will ask before ${rule.tool.action.toLowerCase()} again.`);
  }, [announce, workspace]);

  const editArg = useCallback((key: string, value: string) => {
    const focused = latest.current.focusId;
    if (!focused) return;
    setEdits(current => ({ ...current, [focused]: { ...current[focused], [key]: value } }));
    const arg = latest.current.queue.find(item => item.id === focused)?.args.find(item => item.key === key);
    if (arg) announce(`${arg.label} changed to ${value}. Approve to send the edited value.`);
  }, [announce]);

  const restoreArg = useCallback((key: string) => {
    const focused = latest.current.focusId;
    if (!focused) return;
    setEdits(current => {
      const next = { ...current[focused] };
      delete next[key];
      return { ...current, [focused]: next };
    });
    const arg = latest.current.queue.find(item => item.id === focused)?.args.find(item => item.key === key);
    if (arg) announce(`${arg.label} restored to ${arg.value}.`);
  }, [announce]);

  const focusRequest = useCallback((id: string) => {
    const { focusId: focused, cards: cardMap } = latest.current;
    if (id === focused) return;
    if (focused && !isLive(cardMap[focused])) return;
    if (!isLive(cardMap[id]) || settled.current.has(id)) return;
    setMode("swap");
    setFocusId(id);
    restoreFocus();
  }, [restoreFocus]);

  const pulseKey = useCallback((key: string) => {
    setPressedKey(key);
    schedule(() => setPressedKey(current => current === key ? null : current), 160);
  }, [schedule]);

  const onKeyDown = (event: KeyEventLike) => {
    if (event.defaultPrevented || event.metaKey || event.ctrlKey || event.altKey) return;
    const target = event.target instanceof Element ? event.target : null;
    if (target?.closest("input, textarea, select, [contenteditable='true'], [role='menu'], [role='dialog']")) return;
    const key = event.key.length === 1 ? event.key.toLowerCase() : event.key;
    const focused = latest.current.focusId;
    if (key === "j" || key === "k") {
      const { queue: waiting, cards: cardMap, focusId: current } = latest.current;
      const reachable = waiting.filter(item => item.id === current || (isLive(cardMap[item.id]) && !settled.current.has(item.id)));
      const index = reachable.findIndex(item => item.id === current);
      const next = reachable[Math.max(0, Math.min(reachable.length - 1, index + (key === "j" ? 1 : -1)))];
      if (next) focusRequest(next.id);
    } else if (key === "a" || key === "d") {
      if (!focused) return;
      decide(focused, key === "a" ? "approved" : "denied");
    } else {
      if (key !== "Enter" || !focused || target?.closest("button, a, [role='button']")) return;
      decide(focused, "approved");
    }
    event.preventDefault();
    pulseKey(key === "Enter" ? "enter" : key);
  };

  const keyHandler = useRef(onKeyDown);
  useLayoutEffect(() => {
    keyHandler.current = onKeyDown;
  });

  useEffect(() => {
    const trackPointer = (event: PointerEvent) => {
      inside.current = Boolean(root.current?.contains(event.target as Node));
    };
    const onWindowKeyDown = (event: KeyboardEvent) => {
      if (!inside.current) return;
      if (document.activeElement && document.activeElement !== document.body) return;
      keyHandler.current(event);
    };
    window.addEventListener("pointerdown", trackPointer, true);
    window.addEventListener("keydown", onWindowKeyDown);
    return () => {
      window.removeEventListener("pointerdown", trackPointer, true);
      window.removeEventListener("keydown", onWindowKeyDown);
    };
  }, []);

  const focusedRequest = queue.find(item => item.id === focusId) ?? null;
  const waitingRows = queue.filter(item => item.id !== focusId);
  const pendingCount = queue.filter(item => {
    const phase = cards[item.id]?.phase;
    return phase !== "done" && phase !== "auto";
  }).length;
  const logId = `${rootId}-log`;
  const advancing = mode === "advance" && !reduced;

  return (
    <section ref={root} className={cx(styles.root, className)} aria-labelledby={`${rootId}-title`} onKeyDown={onKeyDown}>
      <header className={styles.header}>
        <h2 id={`${rootId}-title`} className={styles.title}>{title}</h2>
        <span className={styles.count}>
          <AnimatedCounter value={pendingCount} />
          {" pending"}
        </span>
        <RulesButton
          ref={rulesButton}
          rules={ruleList}
          workspace={workspace}
          reduced={reduced}
          bump={bump}
          open={rulesOpen}
          onOpenChange={setRulesOpen}
          onRemove={removeRule}
        />
      </header>
      <div ref={focusArea} className={styles.focusArea} tabIndex={-1} role="region" aria-label="Request in focus" aria-keyshortcuts="Enter a d j k">
        <AutoHeight reduced={reduced} innerClassName={styles.focusStack}>
          <AnimatePresence initial={false} mode="popLayout">
            <motion.div
              key={focusedRequest?.id ?? "empty"}
              className={styles.focusSlide}
              initial={{ opacity: 0, y: reduced ? 0 : advancing ? 14 : 3 }}
              animate={{
                opacity: 1,
                y: 0,
                transition: reduced
                  ? { duration: 0 }
                  : { opacity: { duration: 0.2, ease: ENTER }, y: advancing ? SPRING : { duration: 0.2, ease: ENTER } },
              }}
              exit={{ opacity: 0, y: reduced || !advancing ? 0 : -6, transition: reduced ? { duration: 0 } : EXIT_TWEEN }}
            >
              {focusedRequest ? (
                <FocusCard
                  request={focusedRequest}
                  state={cards[focusedRequest.id] ?? IDLE}
                  edits={edits[focusedRequest.id] ?? {}}
                  now={now}
                  reduced={reduced}
                  workspace={workspace}
                  workspaceMark={workspaceMark ?? (workspace === "Northwind" ? <NorthwindMark aria-hidden="true" /> : undefined)}
                  demo={demo}
                  allowRef={allowButton}
                  onEdit={editArg}
                  onRestore={restoreArg}
                  onDecide={outcome => decide(focusedRequest.id, outcome)}
                  onAlwaysAllow={alwaysAllow}
                />
              ) : (
                <div className={styles.empty}>
                  <CheckCheck className={styles.emptyIcon} size={20} strokeWidth={1.75} aria-hidden="true" />
                  <p className={styles.emptyTitle}>All caught up</p>
                  <p className={styles.emptyText}>Agents ask here when a tool call needs a person. Rules skip the ask.</p>
                  {demo && (
                    <Button
                      variant="secondary"
                      size="sm"
                      onClick={() => {
                        timers.current.forEach(id => window.clearTimeout(id));
                        timers.current.clear();
                        settled.current.clear();
                        simulatedFailureUsed.current = false;
                        afterFlight.current = null;
                        setFlight(null);
                        setQueue(SAMPLE_REQUESTS);
                        setCards({});
                        setEdits({});
                        setRuleList(SAMPLE_RULES);
                        setLog(SAMPLE_RESOLVED);
                        setMode("advance");
                        setFocusId(SAMPLE_REQUESTS[0]?.id ?? null);
                        announce("Sample requests restored.");
                        restoreFocus();
                      }}
                    >
                      Restore sample requests
                    </Button>
                  )}
                </div>
              )}
            </motion.div>
          </AnimatePresence>
        </AutoHeight>
      </div>
      <ul className={styles.queue} aria-label="Waiting requests">
        <AnimatePresence initial={false}>
          {waitingRows.map(request => {
            const auto = cards[request.id]?.phase === "auto";
            return (
              <motion.li
                key={request.id}
                className={styles.queueItem}
                initial={reduced ? { opacity: 0 } : { opacity: 0, height: 0 }}
                animate={{ opacity: 1, height: "auto" }}
                exit={reduced ? { opacity: 0, transition: { duration: 0 } } : { opacity: 0, height: 0 }}
                transition={reduced ? { duration: 0 } : { height: SPRING, opacity: { duration: 0.16 } }}
              >
                <button
                  type="button"
                  className={styles.row}
                  data-auto={auto || undefined}
                  aria-disabled={auto || undefined}
                  onClick={() => focusRequest(request.id)}
                  aria-label={`${request.agent.name}: ${request.summary}. ${auto ? "Allowed by your new rule" : `${request.risk} risk`}`}
                >
                  <span className={styles.rowMarks} aria-hidden="true">
                    <AgentMark agent={request.agent} className={styles.rowMark} />
                    <ToolMark tool={request.tool} className={styles.rowMark} />
                  </span>
                  <span className={styles.rowText}>
                    <span className={styles.rowSummary}>{request.summary}</span>
                    <span className={styles.rowAgent}>{request.agent.name}</span>
                  </span>
                  <Swap id={auto ? "auto" : "risk"} reduced={reduced} className={styles.rowStatus}>
                    {auto ? (
                      <span className={styles.allowed}>
                        <ShieldCheck size={15} strokeWidth={1.75} aria-hidden="true" />
                        <span>
                          Allowed
                          <span className={styles.wideOnly}> by rule</span>
                        </span>
                      </span>
                    ) : (
                      <RiskBadge risk={request.risk} />
                    )}
                  </Swap>
                </button>
              </motion.li>
            );
          })}
        </AnimatePresence>
      </ul>
      <div className={styles.footer}>
        <button type="button" className={styles.logToggle} aria-expanded={logOpen} aria-controls={logId} onClick={() => setLogOpen(open => !open)}>
          <ChevronRight className={styles.chevron} size={16} strokeWidth={1.75} aria-hidden="true" />
          Resolved
          <span className={styles.dot} aria-hidden="true">·</span>
          <AnimatedCounter value={log.length} />
        </button>
        <span className={styles.hints} aria-hidden="true">
          <span className={styles.hint}>
            <Kbd keys="j" size="sm" pressed={pressedKey === "j"} />
            <Kbd keys="k" size="sm" pressed={pressedKey === "k"} />
            Move
          </span>
          <span className={styles.hint}>
            <Kbd keys="a" size="sm" pressed={pressedKey === "a" || pressedKey === "enter"} />
            Approve
          </span>
          <span className={styles.hint}>
            <Kbd keys="d" size="sm" pressed={pressedKey === "d"} />
            Deny
          </span>
        </span>
      </div>
      <AnimatePresence initial={false}>
        {logOpen && (
          <motion.div
            key="log"
            className={styles.logWrap}
            initial={reduced ? { opacity: 0 } : { opacity: 0, height: 0 }}
            animate={{ opacity: 1, height: "auto" }}
            exit={reduced ? { opacity: 0, transition: { duration: 0 } } : { opacity: 0, height: 0 }}
            transition={reduced ? { duration: 0 } : { height: SPRING, opacity: { duration: 0.16 } }}
          >
            <ResolvedLog id={logId} entries={log} reduced={reduced} />
          </motion.div>
        )}
      </AnimatePresence>
      <AnimatePresence>
        {flight && (
          <motion.span
            key={flight.id}
            className={styles.flight}
            aria-hidden="true"
            initial={{ x: flight.from.x, y: flight.from.y, opacity: 0 }}
            animate={{ x: flight.to.x, y: flight.to.y, opacity: [0, 1, 1, 0] }}
            exit={{ opacity: 0, transition: { duration: 0 } }}
            transition={{
              x: { duration: 0.56, ease: FLIGHT_EASE },
              y: { duration: 0.56, ease: FADE_EASE },
              opacity: { duration: 0.56, times: [0, 0.16, 0.8, 1], ease: "linear" },
            }}
            onAnimationComplete={() => afterFlight.current?.()}
          >
            <span className={styles.chip}>
              <ToolMark tool={flight.rule.tool} className={styles.chipMark} />
              {flight.rule.tool.action}
            </span>
          </motion.span>
        )}
      </AnimatePresence>
      <p className={styles.srOnly} role="status" aria-live="polite">{status}</p>
    </section>
  );
}

export default AgentApprovals;
