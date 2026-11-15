"use client";

import * as DialogPrimitive from "@radix-ui/react-dialog";
import * as TooltipPrimitive from "@radix-ui/react-tooltip";
import {
  ChevronRight,
  FileText,
  FlaskConical,
  Folder,
  House,
  Inbox,
  MessageSquareText,
  Route,
  Settings,
  X,
  type LucideIcon,
} from "lucide-react";
import {
  AnimatePresence,
  motion,
  useMotionValue,
  useReducedMotion,
  useTransform,
  type PanInfo,
  type Transition,
  type Variants,
} from "motion/react";
import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";

import { AnimatedCounter } from "../animated-counter/animated-counter";
import { Avatar } from "../avatar/avatar";
import { AvatarGroup } from "../avatar-group/avatar-group";
import { Badge, type BadgeProps } from "../badge/badge";
import { motionTokens } from "../lib/motion-tokens";
import { person, type PersonId } from "../lib/media";
import { Tooltip } from "../tooltip/tooltip";
import styles from "./sidebar.module.css";

type Tone = NonNullable<BadgeProps["tone"]>;

/** The five teammates this workspace drills into, keyed by the short ids the page registry uses. */
const CAST = {
  emma: "emma-collins",
  marcus: "marcus-johnson",
  ava: "ava-mitchell",
  jasmine: "jasmine-brooks",
  sofia: "sofia-ramirez",
} as const satisfies Record<string, PersonId>;

type CastId = keyof typeof CAST;

const teammate = (id: CastId) => person(CAST[id]);

interface PageRow {
  title: string;
  detail: string;
  person: CastId;
  status: string;
  tone: Tone;
  when: string;
}

interface Page {
  title: string;
  parent?: string;
  summary: string;
  team: CastId[];
  stats: { label: string; value: number; suffix?: string }[];
  list: string;
  rows: PageRow[];
}

const PAGES: Record<string, Page> = {
  home: {
    title: "Home",
    summary: "Three reviews need you before Thursday's critique.",
    team: ["marcus", "sofia", "ava"],
    stats: [
      { label: "Assigned to you", value: 12 },
      { label: "In review", value: 3 },
      { label: "Shipped this week", value: 5 },
    ],
    list: "Needs you",
    rows: [
      {
        title: "Address step spacing",
        detail: "Checkout redesign",
        person: "marcus",
        status: "In review",
        tone: "info",
        when: "Today",
      },
      {
        title: "Onboarding empty states",
        detail: "Mobile onboarding",
        person: "sofia",
        status: "In progress",
        tone: "neutral",
        when: "Wed",
      },
      {
        title: "Dark mode token audit",
        detail: "Design system 2.0",
        person: "ava",
        status: "Blocked",
        tone: "warning",
        when: "Fri",
      },
    ],
  },
  inbox: {
    title: "Inbox",
    summary: "Four unread, most of them about the checkout flow.",
    team: ["marcus", "jasmine", "ava"],
    stats: [
      { label: "Unread", value: 4 },
      { label: "Mentions", value: 2 },
      { label: "Review requests", value: 1 },
    ],
    list: "Unread",
    rows: [
      {
        title: "Marcus commented on Address step",
        detail: "Can the postcode field move above the city?",
        person: "marcus",
        status: "Comment",
        tone: "neutral",
        when: "9:42 AM",
      },
      {
        title: "Jasmine asked for your review",
        detail: "Payment sheet, second pass",
        person: "jasmine",
        status: "Review",
        tone: "info",
        when: "8:15 AM",
      },
      {
        title: "Ava mentioned you",
        detail: "Surface levels in the token audit",
        person: "ava",
        status: "Mention",
        tone: "neutral",
        when: "Yesterday",
      },
    ],
  },
  checkout: {
    title: "Checkout redesign",
    parent: "Projects",
    summary: "Fewer steps from bag to confirmation. Beta opens October 14.",
    team: ["marcus", "jasmine", "emma", "ava"],
    stats: [
      { label: "Screens", value: 38 },
      { label: "Open issues", value: 7 },
      { label: "Days to beta", value: 22 },
    ],
    list: "Screens in flight",
    rows: [
      {
        title: "Address step",
        detail: "Autocomplete and postcode order",
        person: "marcus",
        status: "In review",
        tone: "info",
        when: "Today",
      },
      {
        title: "Payment sheet",
        detail: "Apple Pay and saved cards",
        person: "jasmine",
        status: "In progress",
        tone: "neutral",
        when: "Thu",
      },
      {
        title: "Confirmation",
        detail: "Receipt, tracking, and next steps",
        person: "emma",
        status: "Done",
        tone: "success",
        when: "Mon",
      },
    ],
  },
  onboarding: {
    title: "Mobile onboarding",
    parent: "Projects",
    summary: "A shorter first run that ends in a real first project.",
    team: ["sofia", "emma"],
    stats: [
      { label: "Screens", value: 24 },
      { label: "Open issues", value: 4 },
      { label: "Test sessions", value: 9 },
    ],
    list: "Screens in flight",
    rows: [
      {
        title: "Welcome and sign in",
        detail: "Passkeys first, email as fallback",
        person: "sofia",
        status: "Done",
        tone: "success",
        when: "Sep 12",
      },
      {
        title: "Workspace setup",
        detail: "Invite step moved after first project",
        person: "sofia",
        status: "In progress",
        tone: "neutral",
        when: "Wed",
      },
      {
        title: "Empty states",
        detail: "Starter templates for each role",
        person: "emma",
        status: "Not started",
        tone: "neutral",
        when: "Oct 2",
      },
    ],
  },
  system: {
    title: "Design system 2.0",
    parent: "Projects",
    summary: "New tokens, fewer components, and a dark mode that holds up.",
    team: ["ava", "jasmine", "emma"],
    stats: [
      { label: "Components", value: 64 },
      { label: "Tokens", value: 212 },
      { label: "Adoption", value: 71, suffix: "%" },
    ],
    list: "Workstreams",
    rows: [
      {
        title: "Surface levels",
        detail: "Waiting on the contrast review",
        person: "ava",
        status: "Blocked",
        tone: "warning",
        when: "Fri",
      },
      {
        title: "Button variants",
        detail: "Four sizes down to three",
        person: "jasmine",
        status: "In review",
        tone: "info",
        when: "Thu",
      },
      {
        title: "Motion tokens",
        detail: "Springs for panels and highlights",
        person: "emma",
        status: "Done",
        tone: "success",
        when: "Sep 18",
      },
    ],
  },
  roadmap: {
    title: "Roadmap",
    summary: "Four bets this quarter. One needs a decision this week.",
    team: ["marcus", "sofia", "jasmine"],
    stats: [
      { label: "Bets", value: 4 },
      { label: "On track", value: 2 },
      { label: "At risk", value: 1 },
    ],
    list: "Q4 bets",
    rows: [
      {
        title: "Checkout redesign",
        detail: "Beta with 5% of traffic",
        person: "marcus",
        status: "On track",
        tone: "success",
        when: "Oct 14",
      },
      {
        title: "Mobile onboarding",
        detail: "Second usability round",
        person: "sofia",
        status: "On track",
        tone: "success",
        when: "Nov 3",
      },
      {
        title: "Team workspaces",
        detail: "Needs a pricing decision",
        person: "jasmine",
        status: "At risk",
        tone: "warning",
        when: "Dec 1",
      },
    ],
  },
  research: {
    title: "Research",
    summary: "Twelve interviews so far. Checkout friction came up in nine.",
    team: ["ava", "sofia", "emma"],
    stats: [
      { label: "Interviews", value: 12 },
      { label: "Insights", value: 31 },
      { label: "Open questions", value: 6 },
    ],
    list: "Studies",
    rows: [
      {
        title: "Returning shoppers",
        detail: "Six sessions, synthesis ready",
        person: "ava",
        status: "Ready",
        tone: "success",
        when: "Sep 19",
      },
      {
        title: "First run on mobile",
        detail: "Four sessions booked",
        person: "sofia",
        status: "Scheduled",
        tone: "info",
        when: "Sep 29",
      },
      {
        title: "Checkout drop off",
        detail: "Survey with 412 responses",
        person: "emma",
        status: "Analyzing",
        tone: "neutral",
        when: "Oct 1",
      },
    ],
  },
  launch: {
    title: "Q4 launch plan",
    parent: "Pinned",
    summary: "Dates, owners, and what has to be true before each launch.",
    team: ["jasmine", "emma", "sofia", "marcus", "ava"],
    stats: [
      { label: "Milestones", value: 9 },
      { label: "Complete", value: 4 },
      { label: "Weeks left", value: 11 },
    ],
    list: "Next milestones",
    rows: [
      {
        title: "Beta invite list",
        detail: "1,200 customers from the waitlist",
        person: "jasmine",
        status: "Done",
        tone: "success",
        when: "Sep 20",
      },
      {
        title: "Launch review",
        detail: "Go or no go with support and sales",
        person: "emma",
        status: "Scheduled",
        tone: "info",
        when: "Oct 8",
      },
      {
        title: "Support playbook",
        detail: "Macros for payment questions",
        person: "sofia",
        status: "In progress",
        tone: "neutral",
        when: "Oct 10",
      },
    ],
  },
  critique: {
    title: "Critique notes",
    parent: "Pinned",
    summary: "Decisions from Thursday critiques, newest first.",
    team: ["jasmine", "sofia", "ava"],
    stats: [
      { label: "Sessions", value: 14 },
      { label: "Decisions", value: 38 },
      { label: "Follow ups", value: 5 },
    ],
    list: "Recent decisions",
    rows: [
      {
        title: "Payment sheet",
        detail: "Keep saved cards above the fold",
        person: "jasmine",
        status: "Decided",
        tone: "success",
        when: "Sep 18",
      },
      {
        title: "Onboarding tour",
        detail: "Cut the tour to three steps",
        person: "sofia",
        status: "Decided",
        tone: "success",
        when: "Sep 11",
      },
      {
        title: "Surface contrast",
        detail: "Revisit after the token audit",
        person: "ava",
        status: "Follow up",
        tone: "warning",
        when: "Sep 4",
      },
    ],
  },
  settings: {
    title: "Settings",
    summary: "Northline workspace, 14 members on the team plan.",
    team: ["emma", "marcus"],
    stats: [
      { label: "Members", value: 14 },
      { label: "Guests", value: 3 },
      { label: "Integrations", value: 5 },
    ],
    list: "Connected tools",
    rows: [
      {
        title: "Figma sync",
        detail: "Libraries and comments",
        person: "emma",
        status: "Connected",
        tone: "success",
        when: "4 min ago",
      },
      {
        title: "Slack digest",
        detail: "Posts to #design at 9:00 AM",
        person: "marcus",
        status: "Daily",
        tone: "neutral",
        when: "Today",
      },
      {
        title: "Guest access",
        detail: "Three guests, comment only",
        person: "emma",
        status: "Limited",
        tone: "neutral",
        when: "Sep 2",
      },
    ],
  },
};

interface RailChild {
  id: string;
  label: string;
}

interface RailEntry {
  id: string;
  label: string;
  icon?: LucideIcon;
  count?: number;
  children?: RailChild[];
}

interface RailSection {
  id: string;
  heading?: string;
  items: RailEntry[];
}

const SECTIONS: RailSection[] = [
  {
    id: "main",
    items: [
      { id: "home", label: "Home", icon: House },
      { id: "inbox", label: "Inbox", icon: Inbox, count: 4 },
    ],
  },
  {
    id: "workspace",
    heading: "Workspace",
    items: [
      {
        id: "projects",
        label: "Projects",
        icon: Folder,
        children: [
          { id: "checkout", label: "Checkout redesign" },
          { id: "onboarding", label: "Mobile onboarding" },
          { id: "system", label: "Design system 2.0" },
        ],
      },
      { id: "roadmap", label: "Roadmap", icon: Route },
      { id: "research", label: "Research", icon: FlaskConical },
    ],
  },
  {
    id: "pinned",
    heading: "Pinned",
    items: [
      { id: "launch", label: "Q4 launch plan", icon: FileText },
      { id: "critique", label: "Critique notes", icon: MessageSquareText },
    ],
  },
];

const SETTINGS: RailEntry = {
  id: "settings",
  label: "Settings",
  icon: Settings,
};

/** Parents are listed in breadcrumb order; the sizer row measures each label so the crumb can spring to width. */
const PARENTS = [
  ...new Set(
    Object.values(PAGES).flatMap((page) => (page.parent ? [page.parent] : [])),
  ),
];

const bezier = (
  curve: readonly [number, number, number, number],
): [number, number, number, number] => [curve[0], curve[1], curve[2], curve[3]];

const INSTANT: Transition = { duration: 0 };

/** Sidebar text slides out while the rail narrows, and slides back in a beat after it widens. */
function labelTransition(collapsed: boolean, reduce: boolean): Transition {
  if (reduce) return INSTANT;
  return collapsed
    ? { duration: 0.14, ease: bezier(motionTokens.ease.standard) }
    : { duration: 0.24, delay: 0.1, ease: bezier(motionTokens.ease.enter) };
}

/** Counts, dots, dividers, and chevrons follow the rail with a shorter, later pass. */
function metaTransition(collapsed: boolean, reduce: boolean): Transition {
  if (reduce) return INSTANT;
  return collapsed
    ? { duration: 0.2, delay: 0.16, ease: bezier(motionTokens.ease.enter) }
    : { duration: 0.1, ease: bezier(motionTokens.ease.standard) };
}

const swapVariants: Variants = {
  hidden: { opacity: 0, y: 8, filter: `blur(${motionTokens.blur.soft}px)` },
  shown: { opacity: 1, y: 0, filter: "blur(0px)" },
  gone: { opacity: 0, y: -6, filter: `blur(${motionTokens.blur.subtle}px)` },
};

interface RailTipProps {
  enabled: boolean;
  label: string;
  note?: string;
  children: React.ReactElement;
}

/** A right-hand tooltip that only arms while the rail is collapsed, so wide rows keep their labels visible. */
function RailTip({ enabled, label, note, children }: RailTipProps) {
  const [open, setOpen] = useState(false);

  useEffect(() => {
    if (!enabled && open) setOpen(false);
  }, [enabled, open]);

  return (
    <TooltipPrimitive.Root
      open={enabled && open}
      onOpenChange={(next) => setOpen(next && enabled)}
    >
      <TooltipPrimitive.Trigger asChild>{children}</TooltipPrimitive.Trigger>
      <TooltipPrimitive.Portal>
        <TooltipPrimitive.Content
          className={styles.tip}
          side="right"
          sideOffset={14}
          collisionPadding={8}
        >
          {label}
          {note ? <span className={styles.tipNote}>{note}</span> : null}
        </TooltipPrimitive.Content>
      </TooltipPrimitive.Portal>
    </TooltipPrimitive.Root>
  );
}

interface RailItemProps {
  entry: RailEntry;
  nested?: boolean;
  current: boolean;
  lit: boolean;
  expanded?: boolean;
  turned?: boolean;
  controls?: string;
  collapsed: boolean;
  tips: boolean;
  layoutId: string;
  reduce: boolean;
  onClick: () => void;
}

function RailItem({
  entry,
  nested,
  current,
  lit,
  expanded,
  turned,
  controls,
  collapsed,
  tips,
  layoutId,
  reduce,
  onClick,
}: RailItemProps) {
  const Icon = entry.icon;
  const restTransition = labelTransition(collapsed, reduce);
  const detailTransition = metaTransition(collapsed, reduce);

  return (
    <RailTip
      enabled={tips}
      label={entry.label}
      note={entry.count ? `${entry.count} unread` : undefined}
    >
      <button
        type="button"
        className={[
          styles.item,
          nested ? styles.nested : "",
          lit ? styles.lit : "",
        ].join(" ")}
        aria-current={current ? "page" : undefined}
        aria-expanded={expanded}
        aria-controls={controls}
        onClick={onClick}
      >
        {lit ? (
          <motion.span
            layoutId={layoutId}
            className={styles.highlight}
            style={{ borderRadius: 8 }}
            transition={reduce ? INSTANT : motionTokens.spring.morph}
          />
        ) : null}
        {Icon ? (
          <Icon
            className={styles.icon}
            size={16}
            strokeWidth={1.75}
            aria-hidden="true"
          />
        ) : null}
        <motion.span
          className={styles.label}
          initial={false}
          animate={{ opacity: collapsed ? 0 : 1, x: collapsed ? -6 : 0 }}
          transition={restTransition}
        >
          {entry.label}
          {entry.count ? (
            <span className={styles.srOnly}>, {entry.count} unread</span>
          ) : null}
        </motion.span>
        {entry.count ? (
          <>
            <motion.span
              className={styles.count}
              aria-hidden="true"
              initial={false}
              animate={{ opacity: collapsed ? 0 : 1 }}
              transition={restTransition}
            >
              {entry.count}
            </motion.span>
            <motion.span
              className={styles.dot}
              aria-hidden="true"
              initial={false}
              animate={{
                opacity: collapsed ? 1 : 0,
                scale: collapsed ? 1 : 0.3,
              }}
              transition={
                collapsed && !reduce
                  ? { ...motionTokens.spring.snappy, delay: 0.16 }
                  : detailTransition
              }
            />
          </>
        ) : null}
        {turned !== undefined ? (
          <motion.span
            className={styles.chevron}
            aria-hidden="true"
            initial={false}
            animate={{ opacity: collapsed ? 0 : 1, rotate: turned ? 90 : 0 }}
            transition={{
              opacity: restTransition,
              rotate: reduce ? INSTANT : motionTokens.spring.snappy,
            }}
          >
            <ChevronRight size={14} strokeWidth={1.75} />
          </motion.span>
        ) : null}
      </button>
    </RailTip>
  );
}

interface RailNavProps {
  variant: "side" | "sheet";
  baseId: string;
  collapsed: boolean;
  active: string;
  open: Record<string, boolean>;
  reduce: boolean;
  onSelect: (id: string) => void;
  onGroup: (id: string) => void;
}

function RailNav({
  variant,
  baseId,
  collapsed,
  active,
  open,
  reduce,
  onSelect,
  onGroup,
}: RailNavProps) {
  const tips = variant === "side" && collapsed;
  const layoutId = `${variant}-active`;
  const restTransition = labelTransition(collapsed, reduce);
  const detailTransition = metaTransition(collapsed, reduce);
  const heightTransition: Transition = reduce
    ? INSTANT
    : motionTokens.spring.smooth;
  const shared = { collapsed, tips, layoutId, reduce };

  return (
    <motion.nav className={styles.nav} aria-label="Northline" layoutScroll>
      {SECTIONS.map((section) => {
        const headingId = `${baseId}-${variant}-${section.id}`;
        return (
          <div
            key={section.id}
            className={styles.section}
            role={section.heading ? "group" : undefined}
            aria-labelledby={section.heading ? headingId : undefined}
          >
            {section.heading ? (
              <motion.div
                className={styles.heading}
                initial={false}
                animate={{ height: collapsed ? 17 : 32 }}
                transition={heightTransition}
              >
                <motion.span
                  id={headingId}
                  className={styles.headingText}
                  initial={false}
                  animate={{
                    opacity: collapsed ? 0 : 1,
                    x: collapsed ? -6 : 0,
                  }}
                  transition={restTransition}
                >
                  {section.heading}
                </motion.span>
                <motion.span
                  className={styles.divider}
                  aria-hidden="true"
                  initial={false}
                  animate={{
                    opacity: collapsed ? 1 : 0,
                    scaleX: collapsed ? 1 : 0.3,
                  }}
                  transition={detailTransition}
                />
              </motion.div>
            ) : null}
            <ul className={styles.list} role="list">
              {section.items.map((entry) => {
                if (!entry.children) {
                  return (
                    <li key={entry.id}>
                      <RailItem
                        entry={entry}
                        current={active === entry.id}
                        lit={active === entry.id}
                        onClick={() => onSelect(entry.id)}
                        {...shared}
                      />
                    </li>
                  );
                }
                const childId = `${baseId}-${variant}-${entry.id}-list`;
                const expanded = Boolean(open[entry.id]) && !collapsed;
                const holdsActive = entry.children.some(
                  (child) => child.id === active,
                );
                return (
                  <li key={entry.id}>
                    <RailItem
                      entry={entry}
                      current={false}
                      lit={holdsActive && !expanded}
                      expanded={expanded}
                      turned={Boolean(open[entry.id])}
                      controls={childId}
                      onClick={() => onGroup(entry.id)}
                      {...shared}
                    />
                    <motion.ul
                      id={childId}
                      className={styles.children}
                      role="list"
                      inert={!expanded}
                      initial={false}
                      animate={
                        expanded
                          ? {
                              height: "auto",
                              opacity: 1,
                              transitionEnd: { overflow: "visible" },
                            }
                          : { height: 0, opacity: 0, overflow: "hidden" }
                      }
                      transition={{
                        height: heightTransition,
                        opacity: reduce
                          ? INSTANT
                          : {
                              duration: expanded ? 0.24 : 0.14,
                              delay: expanded ? 0.06 : 0,
                              ease: bezier(motionTokens.ease.standard),
                            },
                      }}
                    >
                      {entry.children.map((child) => (
                        <li key={child.id}>
                          <RailItem
                            entry={child}
                            nested
                            current={active === child.id}
                            lit={expanded && active === child.id}
                            onClick={() => onSelect(child.id)}
                            {...shared}
                          />
                        </li>
                      ))}
                    </motion.ul>
                  </li>
                );
              })}
            </ul>
          </div>
        );
      })}
      <ul className={`${styles.list} ${styles.footerList}`} role="list">
        <li>
          <RailItem
            entry={SETTINGS}
            current={active === "settings"}
            lit={active === "settings"}
            onClick={() => onSelect("settings")}
            {...shared}
          />
        </li>
      </ul>
    </motion.nav>
  );
}

interface CollapsibleTextProps {
  collapsed: boolean;
  reduce: boolean;
}

function Brand({ collapsed, reduce }: CollapsibleTextProps) {
  return (
    <div className={styles.brand}>
      <span className={styles.mark} data-logo="" aria-hidden="true">
        <svg viewBox="0 0 24 24">
          <path d="M12 3.5 5.5 19.5 12 15.6z" fill="currentColor" />
          <path d="M12 3.5l6.5 16-6.5-3.9z" fill="currentColor" opacity=".55" />
        </svg>
      </span>
      <motion.span
        className={styles.stack}
        initial={false}
        animate={{ opacity: collapsed ? 0 : 1, x: collapsed ? -6 : 0 }}
        transition={labelTransition(collapsed, reduce)}
      >
        <strong>Northline</strong>
        <span>Product design</span>
      </motion.span>
    </div>
  );
}

function Profile({ collapsed, reduce }: CollapsibleTextProps) {
  return (
    <div className={styles.profile}>
      <Avatar
        name={teammate("emma").name}
        src={teammate("emma").src}
        size="sm"
        status="online"
      />
      <motion.span
        className={styles.stack}
        initial={false}
        animate={{ opacity: collapsed ? 0 : 1, x: collapsed ? -6 : 0 }}
        transition={labelTransition(collapsed, reduce)}
      >
        <strong>Emma Collins</strong>
        <span>Product designer</span>
      </motion.span>
    </div>
  );
}

/** The rail glyph: a panel outline whose divider slides left when the rail is folded. */
function RailGlyph({ collapsed, reduce }: CollapsibleTextProps) {
  return (
    <svg
      width="20"
      height="20"
      viewBox="0 0 20 20"
      fill="none"
      stroke="currentColor"
      strokeWidth="1.6"
      strokeLinecap="round"
      aria-hidden="true"
    >
      <rect x="2.75" y="3.75" width="14.5" height="12.5" rx="3.5" />
      <motion.path
        d="M8.25 4.25v11.5"
        initial={false}
        animate={{ x: collapsed ? -2.5 : 0 }}
        transition={reduce ? INSTANT : motionTokens.spring.smooth}
      />
    </svg>
  );
}

/**
 * Northline's app rail: a collapsible navigation column beside the page it drives.
 *
 * @remarks
 * No props — the block ships its own workspace fixture. The rail folds to 64 px with `[` (or the topbar
 * button) whenever focus is not inside a text field, and returns to 240 px on the next press. Below a 760 px
 * container it starts folded; between 480 px and 640 px it also floats as a sheet, and under 480 px it only
 * exists as the menu button's dialog. The dialog sheet drags closed to the left and hands focus back to the
 * menu button. Every page swap rises in with a short blur; the active row highlight travels between rows on a
 * shared layout spring. Group rows keep the `aria-expanded`/`aria-controls` pair and the collapsed list is
 * `inert`. Section changes are announced through a polite status region.
 */
export function SidebarRail() {
  const baseId = useId();
  const prefersReduced = Boolean(useReducedMotion());
  const [measuring, setMeasuring] = useState(true);
  const reduce = prefersReduced || measuring;

  const [collapsed, setCollapsed] = useState(false);
  const [compact, setCompact] = useState(false);
  const [active, setActive] = useState("checkout");
  const [openGroups, setOpenGroups] = useState<Record<string, boolean>>({
    projects: true,
  });
  const [sheetOpen, setSheetOpen] = useState(false);
  const [section, setSection] = useState<HTMLElement | null>(null);
  const [announcement, setAnnouncement] = useState("");

  const sheetRef = useRef<HTMLDivElement>(null);
  const menuButtonRef = useRef<HTMLButtonElement>(null);
  const draggingRef = useRef(false);
  const statusTimer = useRef(0);

  const page = PAGES[active];
  const sidebarId = `${baseId}-sidebar`;

  const floats = compact && !collapsed;

  useEffect(() => {
    if (!section) return;
    function onKeyDown(event: KeyboardEvent) {
      if (
        event.key !== "[" ||
        event.metaKey ||
        event.ctrlKey ||
        event.altKey ||
        event.defaultPrevented ||
        !section
      ) {
        return;
      }
      const target = event.target as HTMLElement | null;
      if (
        target &&
        (target.isContentEditable ||
          /^(INPUT|TEXTAREA|SELECT)$/.test(target.tagName))
      ) {
        return;
      }
      const box = section.getBoundingClientRect();
      if (!box.width || box.bottom < 0 || box.top > window.innerHeight) return;
      event.preventDefault();
      if (section.clientWidth < 480) {
        setSheetOpen((open) => !open);
        return;
      }
      setCollapsed((value) => !value);
      setAnnouncement(collapsed ? "Sidebar expanded" : "Sidebar collapsed");
    }
    window.addEventListener("keydown", onKeyDown);
    return () => window.removeEventListener("keydown", onKeyDown);
  }, [section, collapsed]);

  const lastWide = useRef<boolean | null>(null);

  useLayoutEffect(() => {
    if (!section) return;
    const measure = () => {
      const width = section.clientWidth;
      if (width >= 480) setSheetOpen(false);
      const nextCompact = width >= 480 && width < 640;
      setCompact(nextCompact);
      const wide = width >= 760;
      if (lastWide.current !== wide) {
        lastWide.current = wide;
        setCollapsed(!wide);
      } else if (nextCompact) {
        setCollapsed(true);
      }
    };
    measure();
    const frame = requestAnimationFrame(() => setMeasuring(false));
    const observer = new ResizeObserver(measure);
    observer.observe(section);
    return () => {
      observer.disconnect();
      cancelAnimationFrame(frame);
    };
  }, [section]);

  useEffect(() => () => window.clearTimeout(statusTimer.current), []);

  const sizerRef = useRef<HTMLSpanElement>(null);
  const [parentWidths, setParentWidths] = useState<Record<string, number>>({});

  useLayoutEffect(() => {
    const node = sizerRef.current;
    if (!node) return;
    const measure = () => {
      setParentWidths(
        Object.fromEntries(
          Array.from(node.children, (child) => [
            (child as HTMLElement).dataset.label ?? "",
            (child as HTMLElement).offsetWidth,
          ]),
        ),
      );
    };
    measure();
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, []);

  function collapseRail() {
    setCollapsed(true);
    setAnnouncement("Sidebar collapsed");
  }

  function selectPage(id: string, fromSheet = false) {
    if (fromSheet && draggingRef.current) return;
    setActive(id);
    setAnnouncement(`${PAGES[id].title} opened`);
    if (fromSheet || floats) {
      window.clearTimeout(statusTimer.current);
      statusTimer.current = window.setTimeout(
        () => {
          if (fromSheet) setSheetOpen(false);
          else setCollapsed(true);
        },
        200 * Number(!reduce),
      );
    }
  }

  function toggleGroup(id: string, fromSheet = false) {
    if (fromSheet && draggingRef.current) return;
    if (collapsed && !fromSheet) {
      setCollapsed(false);
      setOpenGroups((groups) => ({ ...groups, [id]: true }));
      setAnnouncement("Sidebar expanded");
      return;
    }
    setOpenGroups((groups) => ({ ...groups, [id]: !groups[id] }));
  }

  const sheetX = useMotionValue<string | number>("-100%");
  const scrimOpacity = useTransform(sheetX, (value) => {
    const width = sheetRef.current?.offsetWidth || 288;
    const ratio =
      typeof value === "number"
        ? value / width
        : value.endsWith("%")
          ? parseFloat(value) / 100
          : parseFloat(value) / width;
    return Math.min(1, Math.max(0, 1 + (Number.isFinite(ratio) ? ratio : -1)));
  });

  const swapTransition: Transition = reduce
    ? INSTANT
    : {
        duration: motionTokens.duration.standard,
        ease: bezier(motionTokens.ease.enter),
      };
  const listVariants: Variants = {
    shown: {
      transition: { staggerChildren: reduce ? 0 : motionTokens.stagger.item },
    },
  };

  function onSheetDragEnd(
    _event: PointerEvent | MouseEvent | TouchEvent,
    info: PanInfo,
  ) {
    sheetRef.current?.removeAttribute("data-dragging");
    window.setTimeout(() => {
      draggingRef.current = false;
    }, 0);
    if (info.offset.x < -72 || info.velocity.x < -480) setSheetOpen(false);
  }

  return (
    <TooltipPrimitive.Provider delayDuration={250} skipDelayDuration={300}>
      <section
        ref={setSection}
        className={styles.shell}
        aria-label="Northline workspace"
        onKeyDown={(event) => {
          if (event.key === "Escape" && floats && !event.defaultPrevented) {
            event.preventDefault();
            collapseRail();
          }
        }}
      >
        <motion.div
          className={styles.slot}
          initial={false}
          animate={{ width: floats || collapsed ? 64 : 240 }}
          transition={reduce ? INSTANT : motionTokens.spring.smooth}
        >
          <motion.div
            id={sidebarId}
            className={styles.sidebar}
            initial={false}
            animate={{ width: collapsed ? 64 : 240 }}
            transition={reduce ? INSTANT : motionTokens.spring.smooth}
            data-collapsed={collapsed || undefined}
            data-floating={floats || undefined}
          >
            <Brand collapsed={collapsed} reduce={reduce} />
            <RailNav
              variant="side"
              baseId={baseId}
              collapsed={collapsed}
              active={active}
              open={openGroups}
              reduce={reduce}
              onSelect={(id) => selectPage(id)}
              onGroup={(id) => toggleGroup(id)}
            />
            <Profile collapsed={collapsed} reduce={reduce} />
          </motion.div>
        </motion.div>
        <AnimatePresence>
          {floats ? (
            <motion.div
              key="float-scrim"
              className={`${styles.scrim} ${styles.floatScrim}`}
              aria-hidden="true"
              onClick={collapseRail}
              initial={{ opacity: 0 }}
              animate={{ opacity: 1 }}
              exit={{ opacity: 0 }}
              transition={
                reduce
                  ? INSTANT
                  : {
                      duration: motionTokens.duration.standard,
                      ease: bezier(motionTokens.ease.standard),
                    }
              }
            />
          ) : null}
        </AnimatePresence>
        <div className={styles.main}>
          <header className={styles.topbar}>
            <Tooltip
              side="bottom"
              content={
                <span className={styles.shortcut}>
                  {collapsed ? "Expand sidebar" : "Collapse sidebar"}
                  <kbd>[</kbd>
                </span>
              }
            >
              <button
                type="button"
                className={`${styles.iconButton} ${styles.railToggle}`}
                aria-label={collapsed ? "Expand sidebar" : "Collapse sidebar"}
                aria-expanded={!collapsed}
                aria-controls={sidebarId}
                aria-keyshortcuts="["
                onClick={() => {
                  setCollapsed((value) => !value);
                  setAnnouncement(
                    collapsed ? "Sidebar expanded" : "Sidebar collapsed",
                  );
                }}
              >
                <RailGlyph collapsed={collapsed} reduce={reduce} />
              </button>
            </Tooltip>
            <button
              ref={menuButtonRef}
              type="button"
              className={`${styles.iconButton} ${styles.menuButton}`}
              aria-label="Open navigation"
              aria-haspopup="dialog"
              aria-expanded={sheetOpen}
              aria-keyshortcuts="["
              onClick={() => setSheetOpen(true)}
            >
              <RailGlyph collapsed={false} reduce={reduce} />
            </button>
            <nav className={styles.crumbs} aria-label="Breadcrumb">
              <ol>
                <li className={styles.crumbRoot}>Northline</li>
                <AnimatePresence initial={false}>
                  {page.parent ? (
                    <motion.li
                      key="parent"
                      className={styles.crumbParent}
                      initial={{ width: 0, opacity: 0 }}
                      animate={{ width: "auto", opacity: 1 }}
                      exit={{ width: 0, opacity: 0 }}
                      transition={reduce ? INSTANT : motionTokens.spring.morph}
                    >
                      <motion.span
                        className={styles.crumbSwap}
                        initial={false}
                        animate={{ width: parentWidths[page.parent] ?? "auto" }}
                        transition={
                          reduce ? INSTANT : motionTokens.spring.morph
                        }
                      >
                        <AnimatePresence mode="popLayout" initial={false}>
                          <motion.span
                            key={page.parent}
                            variants={swapVariants}
                            initial="hidden"
                            animate="shown"
                            exit="gone"
                            transition={swapTransition}
                          >
                            {page.parent}
                          </motion.span>
                        </AnimatePresence>
                      </motion.span>
                    </motion.li>
                  ) : null}
                </AnimatePresence>
                <li className={styles.crumbCurrent} aria-current="page">
                  <AnimatePresence mode="popLayout" initial={false}>
                    <motion.span
                      key={active}
                      variants={swapVariants}
                      initial="hidden"
                      animate="shown"
                      exit="gone"
                      transition={swapTransition}
                    >
                      {page.title}
                    </motion.span>
                  </AnimatePresence>
                </li>
              </ol>
              <span
                ref={sizerRef}
                className={styles.crumbSizers}
                aria-hidden="true"
              >
                {PARENTS.map((parent) => (
                  <span key={parent} data-label={parent}>
                    {parent}
                  </span>
                ))}
              </span>
            </nav>
            <div className={styles.team}>
              <AvatarGroup
                members={page.team.map((id) => teammate(id))}
                max={3}
                size="sm"
                label="People on this page"
              />
            </div>
          </header>
          <div className={styles.content}>
            <div className={styles.swap}>
              <AnimatePresence mode="popLayout" initial={false}>
                <motion.div
                  key={active}
                  className={styles.intro}
                  variants={swapVariants}
                  initial="hidden"
                  animate="shown"
                  exit="gone"
                  transition={swapTransition}
                >
                  <h2 className={styles.title}>{page.title}</h2>
                  <p className={styles.summary}>{page.summary}</p>
                </motion.div>
              </AnimatePresence>
            </div>
            <div className={styles.stats}>
              {page.stats.map((stat, index) => (
                <div key={index} className={styles.stat}>
                  <AnimatedCounter
                    value={stat.value}
                    label={stat.label}
                    suffix={stat.suffix}
                  />
                </div>
              ))}
            </div>
            <div className={styles.swap}>
              <AnimatePresence mode="popLayout" initial={false}>
                <motion.div
                  key={active}
                  className={styles.listBlock}
                  initial="hidden"
                  animate="shown"
                  exit="gone"
                  variants={listVariants}
                >
                  <motion.h3
                    className={styles.listTitle}
                    variants={swapVariants}
                    transition={swapTransition}
                  >
                    {page.list}
                  </motion.h3>
                  <ul className={styles.rows} role="list">
                    {page.rows.map((row) => (
                      <motion.li
                        key={row.title}
                        className={styles.row}
                        variants={swapVariants}
                        transition={swapTransition}
                      >
                        <Avatar
                          name={teammate(row.person).name}
                          src={teammate(row.person).src}
                          size="sm"
                        />
                        <span className={styles.rowText}>
                          <strong>{row.title}</strong>
                          <span>{row.detail}</span>
                        </span>
                        <Badge tone={row.tone} size="sm">
                          {row.status}
                        </Badge>
                        <span className={styles.when}>{row.when}</span>
                      </motion.li>
                    ))}
                  </ul>
                </motion.div>
              </AnimatePresence>
            </div>
          </div>
          <p className={styles.srOnly} role="status" aria-live="polite">
            {announcement}
          </p>
        </div>
        <DialogPrimitive.Root open={sheetOpen} onOpenChange={setSheetOpen}>
          <AnimatePresence>
            {sheetOpen && section ? (
              <DialogPrimitive.Portal forceMount container={section}>
                <DialogPrimitive.Overlay asChild forceMount>
                  <motion.div
                    className={styles.scrim}
                    style={{ opacity: scrimOpacity }}
                  />
                </DialogPrimitive.Overlay>
                <DialogPrimitive.Content
                  asChild
                  forceMount
                  aria-describedby={undefined}
                  onCloseAutoFocus={(event) => {
                    event.preventDefault();
                    menuButtonRef.current?.focus({ preventScroll: true });
                  }}
                  onOpenAutoFocus={(event) => {
                    const current = sheetRef.current?.querySelector(
                      "[aria-current='page']",
                    );
                    if (current && !current.closest("[inert]")) {
                      event.preventDefault();
                      (current as HTMLElement).focus();
                    }
                  }}
                >
                  <motion.div
                    ref={sheetRef}
                    className={styles.sheet}
                    style={{ x: sheetX }}
                    initial={{ x: "-100%" }}
                    animate={{ x: 0 }}
                    exit={{ x: "-100%" }}
                    transition={reduce ? INSTANT : motionTokens.spring.smooth}
                    drag={reduce ? false : "x"}
                    dragConstraints={{ left: 0, right: 0 }}
                    dragElastic={{ left: 0.9, right: 0.04 }}
                    dragMomentum={false}
                    onDragStart={() => {
                      draggingRef.current = true;
                      sheetRef.current?.setAttribute("data-dragging", "");
                    }}
                    onDragEnd={onSheetDragEnd}
                  >
                    <DialogPrimitive.Title className={styles.srOnly}>
                      Navigation
                    </DialogPrimitive.Title>
                    <div className={styles.sheetHead}>
                      <Brand collapsed={false} reduce={reduce} />
                      <DialogPrimitive.Close asChild>
                        <button
                          type="button"
                          className={styles.iconButton}
                          aria-label="Close navigation"
                        >
                          <X size={18} strokeWidth={1.75} aria-hidden="true" />
                        </button>
                      </DialogPrimitive.Close>
                    </div>
                    <RailNav
                      variant="sheet"
                      baseId={baseId}
                      collapsed={false}
                      active={active}
                      open={openGroups}
                      reduce={reduce}
                      onSelect={(id) => selectPage(id, true)}
                      onGroup={(id) => toggleGroup(id, true)}
                    />
                    <Profile collapsed={false} reduce={reduce} />
                  </motion.div>
                </DialogPrimitive.Content>
              </DialogPrimitive.Portal>
            ) : null}
          </AnimatePresence>
        </DialogPrimitive.Root>
      </section>
    </TooltipPrimitive.Provider>
  );
}

export default SidebarRail;
