"use client";

import { useEffect, useId, useLayoutEffect, useRef, useState } from "react";
import type { ReactNode, RefObject } from "react";
import { AnimatePresence, animate, LayoutGroup, motion, useMotionValue, useReducedMotion } from "motion/react";
import type { MotionValue, Variants } from "motion/react";
import type { LucideIcon } from "lucide-react";
import { Bell, CreditCard, ImageUp, Laptop, Monitor, ShieldCheck, Smartphone, Tablet, TriangleAlert, UserRound } from "lucide-react";
import { AnimatedCounter } from "../animated-counter/animated-counter";
import { Avatar } from "../avatar/avatar";
import { Button } from "../button/button";
import { Input } from "../input/input";
import SegmentedControl from "../segmented-control/segmented-control";
import { Select } from "../select/select";
import { Switch } from "../switch/switch";
import { TextMorph } from "../text-morph/text-morph";
import { motionTokens } from "../lib/motion-tokens";
import styles from "./settings-page.module.css";

type SectionId = "profile" | "notifications" | "security" | "billing";

interface Section {
  id: SectionId;
  label: string;
  description: string;
  icon: LucideIcon;
}

interface SettingsState {
  photo: string | null;
  name: string;
  title: string;
  email: string;
  timezone: string;
  mentions: boolean;
  summary: boolean;
  news: boolean;
  messages: boolean;
  reminders: boolean;
  quiet: boolean;
  quietHours: string;
  twoStep: boolean;
  alerts: boolean;
  timeout: string;
  signedOut: string[];
  cycle: "monthly" | "yearly";
  billingEmail: string;
  company: string;
  country: string;
  vat: string;
}

type FieldKey = keyof SettingsState;
type Status = "idle" | "saving" | "saved";

const sections: Section[] = [
  { id: "profile", label: "Profile", description: "How you appear to teammates across Northwind.", icon: UserRound },
  { id: "notifications", label: "Notifications", description: "Choose what reaches you, and when.", icon: Bell },
  { id: "security", label: "Security", description: "Protect your account and review where you are signed in.", icon: ShieldCheck },
  { id: "billing", label: "Billing", description: "Your plan, payment method, and invoice details.", icon: CreditCard },
];

const defaults: SettingsState = {
  photo: "/media/people/emma-collins.svg",
  name: "Emma Collins",
  title: "Product designer",
  email: "emma@northwind.studio",
  timezone: "lisbon",
  mentions: true,
  summary: true,
  news: false,
  messages: true,
  reminders: false,
  quiet: false,
  quietHours: "22-7",
  twoStep: true,
  alerts: true,
  timeout: "8h",
  signedOut: [],
  cycle: "monthly",
  billingEmail: "billing@northwind.studio",
  company: "Northwind Studio Lda.",
  country: "pt",
  vat: "PT 516 283 947",
};

/** Which section owns each field, so a failed save can jump to the first thing that is wrong. */
const fieldSection: Record<FieldKey, SectionId> = {
  photo: "profile",
  name: "profile",
  title: "profile",
  email: "profile",
  timezone: "profile",
  mentions: "notifications",
  summary: "notifications",
  news: "notifications",
  messages: "notifications",
  reminders: "notifications",
  quiet: "notifications",
  quietHours: "notifications",
  twoStep: "security",
  alerts: "security",
  timeout: "security",
  signedOut: "security",
  cycle: "billing",
  billingEmail: "billing",
  company: "billing",
  country: "billing",
  vat: "billing",
};

const timezones = [
  { value: "lisbon", label: "Lisbon (GMT+1)" },
  { value: "london", label: "London (GMT+1)" },
  { value: "berlin", label: "Berlin (GMT+2)" },
  { value: "new-york", label: "New York (GMT\u22124)" },
  { value: "san-francisco", label: "San Francisco (GMT\u22127)" },
  { value: "tokyo", label: "Tokyo (GMT+9)" },
];

const quietHours = [
  { value: "21-8", label: "9 PM to 8 AM" },
  { value: "22-7", label: "10 PM to 7 AM" },
  { value: "23-6", label: "11 PM to 6 AM" },
  { value: "0-8", label: "Midnight to 8 AM" },
];

const timeouts = [
  { value: "1h", label: "After 1 hour" },
  { value: "8h", label: "After 8 hours" },
  { value: "24h", label: "After 24 hours" },
  { value: "7d", label: "After 7 days" },
];

const countries = [
  { value: "pt", label: "Portugal" },
  { value: "de", label: "Germany" },
  { value: "nl", label: "Netherlands" },
  { value: "gb", label: "United Kingdom" },
  { value: "us", label: "United States" },
];

const cycles = [
  { value: "monthly", label: "Monthly" },
  { value: "yearly", label: "Yearly" },
];

interface Session {
  id: string;
  device: string;
  app: string;
  place: string;
  seen: string;
  icon: LucideIcon;
  current?: boolean;
}

const sessions: Session[] = [
  { id: "mac", device: "MacBook Pro", app: "Safari", place: "Lisbon, Portugal", seen: "Active now", icon: Laptop, current: true },
  { id: "iphone", device: "iPhone 16", app: "Northwind for iOS", place: "Lisbon, Portugal", seen: "2 hours ago", icon: Smartphone },
  { id: "ipad", device: "iPad Air", app: "Safari", place: "Porto, Portugal", seen: "Yesterday", icon: Tablet },
  { id: "windows", device: "Windows PC", app: "Chrome", place: "Berlin, Germany", seen: "12 days ago", icon: Monitor },
];

const emailPattern = /^[^\s@]+@[^\s@]+\.[^\s@]{2,}$/;
const enterEase = [...motionTokens.ease.enter] as [number, number, number, number];
const standardEase = [...motionTokens.ease.standard] as [number, number, number, number];
const blur = (px: number) => `blur(${px}px)`;

/** Pane entry and exit: a short perpendicular slide that reads as the next page arriving. */
const paneVariants: Variants = {
  enter: ({ direction, axis, reduce }: { direction: number; axis: "x" | "y"; reduce: boolean }) =>
    reduce ? { opacity: 0 } : { opacity: 0, ...(axis === "x" ? { x: 16 * direction } : { y: 14 * direction }), filter: blur(motionTokens.blur.subtle) },
  center: ({ reduce }: { reduce: boolean }) => ({
    opacity: 1,
    x: 0,
    y: 0,
    filter: blur(0),
    transition: reduce
      ? { duration: motionTokens.duration.instant }
      : {
          x: motionTokens.spring.smooth,
          y: motionTokens.spring.smooth,
          opacity: { duration: motionTokens.duration.standard, ease: enterEase },
          filter: { duration: motionTokens.duration.standard, ease: enterEase },
        },
  }),
  exit: ({ direction, axis, reduce }: { direction: number; axis: "x" | "y"; reduce: boolean }) =>
    reduce
      ? { opacity: 0, transition: { duration: motionTokens.duration.instant } }
      : {
          opacity: 0,
          ...(axis === "x" ? { x: -10 * direction } : { y: -8 * direction }),
          filter: blur(motionTokens.blur.subtle),
          transition: { duration: motionTokens.duration.fast, ease: standardEase },
        },
};

const noteVariants: Variants = {
  initial: { opacity: 0, y: 6, filter: blur(motionTokens.blur.soft) },
  animate: { opacity: 1, y: 0, filter: blur(0) },
  exit: { opacity: 0, y: -4, filter: blur(motionTokens.blur.subtle) },
};

/** A block that expands its height when it appears, used for conditional fields and warnings. */
function Reveal({ show, className, children }: { show: boolean; className?: string; children: ReactNode }) {
  const reduced = useReducedMotion();
  return (
    <AnimatePresence initial={false}>
      {show ? (
        <motion.div
          className={styles.reveal}
          initial={{ height: 0, opacity: 0 }}
          animate={{ height: "auto", opacity: 1 }}
          exit={{ height: 0, opacity: 0 }}
          transition={reduced ? { duration: 0 } : { height: motionTokens.spring.smooth, opacity: { duration: motionTokens.duration.standard } }}
        >
          <div className={className}>{children}</div>
        </motion.div>
      ) : null}
    </AnimatePresence>
  );
}

/** A titled group of rows. */
function Group({ id, title, children, footer }: { id: string; title: string; children: ReactNode; footer?: ReactNode }) {
  return (
    <section className={styles.group} aria-labelledby={id}>
      <h4 id={id}>{title}</h4>
      <div className={styles.rows}>{children}</div>
      {footer}
    </section>
  );
}

/** One switch row: label, description, and whatever else the row needs. */
function Row({ id, label, description, checked, onChange, children }: { id: string; label: string; description: string; checked: boolean; onChange: (checked: boolean) => void; children?: ReactNode }) {
  return (
    <div className={styles.row}>
      <div className={styles.rowText}>
        <label htmlFor={id}>{label}</label>
        <p id={`${id}-hint`}>{description}</p>
        {children}
      </div>
      <Switch id={id} checked={checked} onCheckedChange={onChange} aria-describedby={`${id}-hint`} />
    </div>
  );
}

/** The floating save bar: a pill that measures its own content and reshapes between states. */
function SaveBar({
  state,
  count,
  onDiscard,
  onSave,
  shake,
  barRef,
}: {
  state: "saved" | "edit" | "saving";
  count: number;
  onDiscard: () => void;
  onSave: () => void;
  shake: MotionValue<number>;
  barRef: RefObject<HTMLDivElement | null>;
}) {
  const reduced = useReducedMotion();
  const content = useRef<HTMLDivElement>(null);
  const width = useMotionValue(52);
  const height = useMotionValue(52);
  useLayoutEffect(() => {
    const node = content.current;
    if (!node) return;
    const measure = () => {
      const nextWidth = node.offsetWidth + 2;
      const nextHeight = node.offsetHeight + 2;
      if (reduced) {
        width.jump(nextWidth);
        height.jump(nextHeight);
      } else {
        animate(width, nextWidth, motionTokens.spring.morph);
        animate(height, nextHeight, motionTokens.spring.morph);
      }
    };
    measure();
    if (typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(measure);
    observer.observe(node);
    return () => observer.disconnect();
  }, [reduced, width, height]);

  return (
    <motion.div
      ref={barRef}
      className={styles.bar}
      role="group"
      aria-label={state === "saved" ? "Save status" : "Unsaved changes"}
      style={{ width, height, x: shake }}
      initial={reduced ? { opacity: 0 } : { opacity: 0, y: 28, filter: blur(motionTokens.blur.soft) }}
      animate={{ opacity: 1, y: 0, filter: blur(0) }}
      exit={
        reduced
          ? { opacity: 0, transition: { duration: motionTokens.duration.instant } }
          : {
              opacity: 0,
              y: 20,
              width: 52,
              height: 52,
              filter: blur(motionTokens.blur.soft),
              transition: {
                width: motionTokens.spring.smooth,
                height: motionTokens.spring.smooth,
                y: { ...motionTokens.spring.smooth, delay: 0.06 },
                opacity: { duration: motionTokens.duration.exit, delay: 0.1 },
                filter: { duration: motionTokens.duration.exit, delay: 0.1 },
              },
            }
      }
      transition={reduced ? { duration: motionTokens.duration.instant } : { y: motionTokens.spring.smooth, opacity: { duration: motionTokens.duration.fast }, filter: { duration: motionTokens.duration.standard } }}
    >
      <div ref={content} className={styles.barContent}>
        <AnimatePresence mode="popLayout" initial={false}>
          {state === "saved" ? (
            <motion.div
              key="saved"
              className={styles.barSaved}
              initial={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.96, filter: blur(motionTokens.blur.soft) }}
              animate={{ opacity: 1, scale: 1, filter: blur(0) }}
              exit={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.96, filter: blur(motionTokens.blur.soft) }}
              transition={{ duration: reduced ? motionTokens.duration.instant : motionTokens.duration.standard, ease: enterEase }}
            >
              <svg width={18} height={18} viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth={2} strokeLinecap="round" strokeLinejoin="round" aria-hidden="true">
                <motion.path
                  d="M5 12.5 9.5 17 19 7.5"
                  initial={{ pathLength: reduced ? 1 : 0 }}
                  animate={{ pathLength: 1 }}
                  transition={{ duration: reduced ? 0 : 0.34, delay: reduced ? 0 : 0.08, ease: enterEase }}
                />
              </svg>
              <span>Changes saved</span>
            </motion.div>
          ) : (
            <motion.div
              key="edit"
              className={styles.barEdit}
              initial={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.96, filter: blur(motionTokens.blur.soft) }}
              animate={{ opacity: 1, scale: 1, filter: blur(0) }}
              exit={reduced ? { opacity: 0 } : { opacity: 0, scale: 0.96, filter: blur(motionTokens.blur.soft) }}
              transition={{ duration: reduced ? motionTokens.duration.instant : motionTokens.duration.standard, ease: enterEase }}
            >
              <span className={styles.barCount}>
                <span className={styles.inherit}>
                  <AnimatedCounter value={count} />
                </span>
                <span className={styles.unsaved}>{"\u00a0unsaved"}</span>
                {"\u00a0"}
                <TextMorph>{count === 1 ? "change" : "changes"}</TextMorph>
              </span>
              <Button type="button" variant="ghost" size="sm" disabled={state === "saving"} onClick={onDiscard}>
                Discard
              </Button>
              <Button type="button" size="sm" loading={state === "saving"} onClick={onSave}>
                {state === "saving" ? (
                  "Saving"
                ) : (
                  <span>
                    Save<span className={styles.long}>{"\u00a0changes"}</span>
                  </span>
                )}
              </Button>
            </motion.div>
          )}
        </AnimatePresence>
      </div>
    </motion.div>
  );
}

/**
 * Settings page — a sidebar of sections and a scrolling pane with profile, notification, security
 * and billing forms. Edits stay in a draft until saved; the floating bar counts what changed, the
 * sidebar marks which sections are dirty, and Cmd/Ctrl+S saves too.
 *
 * @remarks No props: the block is self-contained and starts from its sample workspace.
 */
export function SettingsPage() {
  const titleId = useId();
  const groupId = useId();
  const reduced = useReducedMotion();
  const [tab, setTab] = useState<SectionId>("profile");
  const [transition, setTransition] = useState<{ direction: number; axis: "x" | "y" }>({ direction: 1, axis: "y" });
  const [baseline, setBaseline] = useState(defaults);
  const [draft, setDraft] = useState(defaults);
  const [status, setStatus] = useState<Status>("idle");
  const [revealed, setRevealed] = useState<FieldKey[]>([]);
  const [shake, setShake] = useState<{ tick: number; keys: FieldKey[] }>({ tick: 0, keys: [] });
  const [photoError, setPhotoError] = useState("");
  const [liveMessage, setLiveMessage] = useState("");
  const navList = useRef<HTMLUListElement>(null);
  const navItems = useRef<(HTMLButtonElement | null)[]>([]);
  const scroller = useRef<HTMLDivElement>(null);
  const heading = useRef<HTMLHeadingElement>(null);
  const bar = useRef<HTMLDivElement>(null);
  const fileInput = useRef<HTMLInputElement>(null);
  const pendingFocus = useRef<string | null>(null);
  const timers = useRef(new Set<number>());
  const objectUrls = useRef(new Set<string>());
  const shakeX = useMotionValue(0);

  const changed = (Object.keys(draft) as FieldKey[]).filter((key) => key !== "signedOut" && draft[key] !== baseline[key]);
  const signedOutChanges =
    draft.signedOut.filter((id) => !baseline.signedOut.includes(id)).length + baseline.signedOut.filter((id) => !draft.signedOut.includes(id)).length;
  const dirtyCount = changed.length + signedOutChanges;

  const errors: Partial<Record<FieldKey, string>> = {};
  if (!draft.name.trim()) errors.name = "Enter your full name";
  if (!emailPattern.test(draft.email.trim())) errors.email = "Enter a valid email address";
  if (!emailPattern.test(draft.billingEmail.trim())) errors.billingEmail = "Enter a valid billing email";

  const barState: "saved" | "edit" | "saving" | null = status === "saving" ? "saving" : dirtyCount ? "edit" : status === "saved" ? "saved" : null;
  const active = sections.find((section) => section.id === tab) ?? sections[0];
  const visibleSessions = sessions.filter((session) => !draft.signedOut.includes(session.id));
  const price = draft.cycle === "yearly" ? 1440 : 144;
  const paneContext = { ...transition, reduce: !!reduced };

  function later(fn: () => void, ms: number) {
    const id = window.setTimeout(() => {
      timers.current.delete(id);
      fn();
    }, ms);
    timers.current.add(id);
  }

  function update<K extends FieldKey>(field: K, value: SettingsState[K]) {
    setDraft((current) => ({ ...current, [field]: value }));
  }

  useEffect(() => {
    const timeouts = timers.current;
    const urls = objectUrls.current;
    return () => {
      timeouts.forEach((id) => window.clearTimeout(id));
      urls.forEach((url) => URL.revokeObjectURL(url));
    };
  }, []);

  useEffect(() => {
    if (barState === "edit" || barState === "saving") return;
    if (bar.current?.contains(document.activeElement)) heading.current?.focus({ preventScroll: true });
  }, [barState]);

  const reveal = (field: FieldKey) => setRevealed((current) => (current.includes(field) ? current : [...current, field]));
  const errorFor = (field: FieldKey) => (revealed.includes(field) ? errors[field] : undefined);
  const shakeFor = (field: FieldKey) => (shake.keys.includes(field) ? (shake.tick % 2 ? styles.revertA : styles.revertB) : undefined);
  const focusById = (id: string) => requestAnimationFrame(() => requestAnimationFrame(() => document.getElementById(id)?.focus()));

  function showSection(next: SectionId, focusId?: string) {
    if (next === tab) {
      if (focusId) focusById(focusId);
      return;
    }
    const from = sections.findIndex((section) => section.id === tab);
    const to = sections.findIndex((section) => section.id === next);
    const list = navList.current;
    const horizontal = !!list && getComputedStyle(list).flexDirection === "row";
    setTransition({ direction: to > from ? 1 : -1, axis: horizontal ? "x" : "y" });
    setTab(next);
    setShake((current) => (current.keys.length ? { ...current, keys: [] } : current));
    pendingFocus.current = focusId ?? null;
    if (horizontal) navItems.current[to]?.scrollIntoView({ block: "nearest", inline: "nearest", behavior: reduced ? "auto" : "smooth" });
  }

  function save() {
    if (!dirtyCount || status === "saving") return;
    const failing = Object.keys(errors) as FieldKey[];
    if (failing.length) {
      setRevealed((current) => [...new Set([...current, ...failing])]);
      setLiveMessage(`${errors[failing[0]]} before saving`);
      if (!reduced) animate(shakeX, [0, -7, 6, -4, 2, 0], { duration: 0.42, ease: "easeOut" });
      showSection(fieldSection[failing[0]], `${titleId}-${failing[0]}`);
      return;
    }
    setStatus("saving");
    setLiveMessage("Saving changes");
    later(() => {
      setBaseline(draft);
      setStatus("saved");
      setRevealed([]);
      setLiveMessage(`${dirtyCount} ${dirtyCount === 1 ? "change" : "changes"} saved.`);
      later(() => setStatus((current) => (current === "saved" ? "idle" : current)), 1800);
    }, 900);
  }

  function discard() {
    if (status === "saving") return;
    setShake((current) => ({ tick: current.tick + 1, keys: changed }));
    setDraft(baseline);
    setRevealed([]);
    setPhotoError("");
    setLiveMessage(`${dirtyCount} ${dirtyCount === 1 ? "change" : "changes"} discarded`);
  }

  const unsavedFor = (section: SectionId) =>
    changed.filter((field) => fieldSection[field] === section).length + (section === "security" ? signedOutChanges : 0);

  return (
    <section
      className={styles.root}
      aria-labelledby={`${titleId}-title`}
      onKeyDown={(event) => {
        if ((event.metaKey || event.ctrlKey) && event.key.toLowerCase() === "s") {
          event.preventDefault();
          save();
        }
      }}
    >
      <div className={styles.settings}>
        <aside className={styles.sidebar}>
          <div className={styles.sidebarHead}>
            <h2 id={`${titleId}-title`}>Settings</h2>
            <span className={styles.headAvatar}>
              <Avatar name={draft.name} src={draft.photo ?? undefined} size="sm" />
            </span>
          </div>
          <LayoutGroup id={groupId}>
            <nav aria-label="Settings sections">
              <motion.ul ref={navList} className={styles.navList} layoutScroll>
                {sections.map((section, index) => {
                  const Icon = section.icon;
                  const count = unsavedFor(section.id);
                  return (
                    <li key={section.id}>
                      <button
                        ref={(node) => {
                          navItems.current[index] = node;
                        }}
                        type="button"
                        className={styles.navItem}
                        aria-current={tab === section.id ? "page" : undefined}
                        onClick={() => showSection(section.id)}
                        onKeyDown={(event) => {
                          const step: Record<string, number> = { ArrowDown: 1, ArrowRight: 1, ArrowUp: -1, ArrowLeft: -1 };
                          const next =
                            event.key in step
                              ? (index + step[event.key] + sections.length) % sections.length
                              : event.key === "Home"
                                ? 0
                                : event.key === "End"
                                  ? sections.length - 1
                                  : -1;
                          if (next < 0) return;
                          event.preventDefault();
                          navItems.current[next]?.focus();
                          showSection(sections[next].id);
                        }}
                      >
                        {tab === section.id ? (
                          <motion.span
                            layoutId="settings-section"
                            layoutDependency={tab}
                            className={styles.highlight}
                            transition={reduced ? { duration: 0 } : motionTokens.spring.morph}
                            aria-hidden="true"
                          />
                        ) : null}
                        <Icon size={16} strokeWidth={1.75} aria-hidden="true" />
                        <span className={styles.navLabel}>{section.label}</span>
                        <AnimatePresence initial={false}>
                          {count > 0 ? (
                            <motion.span
                              key="dot"
                              className={styles.navDot}
                              aria-hidden="true"
                              initial={{ opacity: 0, scale: 0.2 }}
                              animate={{ opacity: 1, scale: 1 }}
                              exit={{ opacity: 0, scale: 0.2 }}
                              transition={reduced ? { duration: 0 } : motionTokens.spring.snappy}
                            />
                          ) : null}
                        </AnimatePresence>
                        {count > 0 ? <span className={styles.srOnly}>, {count} unsaved</span> : null}
                      </button>
                    </li>
                  );
                })}
              </motion.ul>
            </nav>
          </LayoutGroup>
          <div className={styles.account}>
            <Avatar name={draft.name} src={draft.photo ?? undefined} size="md" />
            <span>
              <strong>{draft.name}</strong>
              <small>Team plan · Admin</small>
            </span>
          </div>
        </aside>
        <div className={styles.main}>
          <div ref={scroller} className={styles.scroller} role="region" aria-labelledby={`${titleId}-heading`}>
            <AnimatePresence
              mode="wait"
              initial={false}
              custom={paneContext}
              onExitComplete={() => {
                scroller.current?.scrollTo({ top: 0 });
                if (pendingFocus.current) {
                  focusById(pendingFocus.current);
                  pendingFocus.current = null;
                }
              }}
            >
              <motion.div key={tab} className={styles.pane} custom={paneContext} variants={paneVariants} initial="enter" animate="center" exit="exit">
                <header className={styles.paneHead}>
                  <h3 id={`${titleId}-heading`} ref={heading} tabIndex={-1}>
                    {active.label}
                  </h3>
                  <p>{active.description}</p>
                </header>
                {tab === "profile" ? (
                  <>
                    <div className={styles.identity}>
                      <div className={styles.photo}>
                        <AnimatePresence initial={false} mode="popLayout">
                          <motion.span
                            key={draft.photo ?? "initials"}
                            className={styles.photoFrame}
                            initial={{ opacity: 0, scale: 0.9, filter: blur(motionTokens.blur.soft) }}
                            animate={{ opacity: 1, scale: 1, filter: blur(0) }}
                            exit={{ opacity: 0, scale: 1.04, filter: blur(motionTokens.blur.soft) }}
                            transition={
                              reduced
                                ? { duration: 0 }
                                : { scale: motionTokens.spring.smooth, opacity: { duration: motionTokens.duration.standard }, filter: { duration: motionTokens.duration.standard } }
                            }
                          >
                            <Avatar name={draft.name.trim() || baseline.name} src={draft.photo ?? undefined} size="xl" />
                          </motion.span>
                        </AnimatePresence>
                      </div>
                      <div className={styles.identityText}>
                        <strong>{draft.name.trim() || "Your name"}</strong>
                        <span>
                          {draft.title.trim() || "No job title"}
                          {" · Northwind Studio"}
                        </span>
                        <div className={styles.photoActions}>
                          <Button id={`${titleId}-change-photo`} type="button" variant="secondary" size="sm" onClick={() => fileInput.current?.click()}>
                            <ImageUp size={15} strokeWidth={1.8} aria-hidden="true" />
                            Change photo
                          </Button>
                          {draft.photo ? (
                            <Button
                              type="button"
                              variant="ghost"
                              size="sm"
                              onClick={() => {
                                update("photo", null);
                                setPhotoError("");
                                setLiveMessage("Photo removed. Your initials will show instead.");
                                focusById(`${titleId}-change-photo`);
                              }}
                            >
                              Remove
                            </Button>
                          ) : null}
                        </div>
                        <input
                          ref={fileInput}
                          type="file"
                          accept="image/png,image/jpeg,image/webp"
                          hidden
                          onChange={(event) => {
                            const file = event.target.files?.[0];
                            event.target.value = "";
                            if (!file) return;
                            if (!/^image\/(png|jpeg|webp)$/.test(file.type)) {
                              setPhotoError("Choose a JPG, PNG, or WebP image");
                              return;
                            }
                            if (file.size > 5242880) {
                              setPhotoError("Choose an image under 5 MB");
                              return;
                            }
                            const url = URL.createObjectURL(file);
                            objectUrls.current.add(url);
                            setPhotoError("");
                            update("photo", url);
                            setLiveMessage("New photo ready to save");
                          }}
                        />
                        <Reveal show={!!photoError} className={styles.photoError}>
                          <span role="alert">{photoError}</span>
                        </Reveal>
                      </div>
                    </div>
                    <div className={styles.fields}>
                      <Input
                        id={`${titleId}-name`}
                        label="Full name"
                        value={draft.name}
                        autoComplete="name"
                        className={shakeFor("name")}
                        error={errorFor("name")}
                        onBlur={() => reveal("name")}
                        onChange={(event) => update("name", event.target.value)}
                      />
                      <Input
                        id={`${titleId}-title-field`}
                        label="Job title"
                        value={draft.title}
                        autoComplete="organization-title"
                        className={shakeFor("title")}
                        onChange={(event) => update("title", event.target.value)}
                      />
                      <Input
                        id={`${titleId}-email`}
                        label="Email"
                        type="email"
                        value={draft.email}
                        autoComplete="email"
                        spellCheck={false}
                        description="Used for sign-in and notifications."
                        className={shakeFor("email")}
                        error={errorFor("email")}
                        onBlur={() => reveal("email")}
                        onChange={(event) => update("email", event.target.value)}
                      />
                      <Select label="Time zone" value={draft.timezone} options={timezones} onValueChange={(value) => update("timezone", value)} />
                    </div>
                  </>
                ) : null}
                {tab === "notifications" ? (
                  <>
                    <Group id={`${titleId}-email-group`} title="Email">
                      <Row
                        id={`${titleId}-mentions`}
                        label="Comments and mentions"
                        description="Replies to your comments and anyone who mentions you."
                        checked={draft.mentions}
                        onChange={(value) => update("mentions", value)}
                      />
                      <Row
                        id={`${titleId}-summary`}
                        label="Weekly summary"
                        description="A Monday recap of what moved in your projects."
                        checked={draft.summary}
                        onChange={(value) => update("summary", value)}
                      />
                      <Row
                        id={`${titleId}-news`}
                        label="Product news"
                        description="New features and changes, about once a month."
                        checked={draft.news}
                        onChange={(value) => update("news", value)}
                      />
                    </Group>
                    <Group id={`${titleId}-device-group`} title="On your devices">
                      <Row
                        id={`${titleId}-messages`}
                        label="Direct messages"
                        description="Messages sent to you or to a group you are in."
                        checked={draft.messages}
                        onChange={(value) => update("messages", value)}
                      />
                      <Row
                        id={`${titleId}-reminders`}
                        label="Reminders"
                        description="Due dates and meetings, 15 minutes ahead."
                        checked={draft.reminders}
                        onChange={(value) => update("reminders", value)}
                      />
                    </Group>
                    <Group
                      id={`${titleId}-quiet-group`}
                      title="Quiet hours"
                      footer={
                        <Reveal show={draft.quiet} className={styles.revealField}>
                          <Select label="Pause between" value={draft.quietHours} options={quietHours} onValueChange={(value) => update("quietHours", value)} />
                        </Reveal>
                      }
                    >
                      <Row
                        id={`${titleId}-quiet`}
                        label="Pause overnight"
                        description="Hold notifications and deliver them together in the morning."
                        checked={draft.quiet}
                        onChange={(value) => update("quiet", value)}
                      />
                    </Group>
                  </>
                ) : null}
                {tab === "security" ? (
                  <>
                    <Group id={`${titleId}-signin-group`} title="Sign-in">
                      <Row
                        id={`${titleId}-two-step`}
                        label="Two-step verification"
                        description="Ask for a code from your authenticator app on new devices."
                        checked={draft.twoStep}
                        onChange={(value) => update("twoStep", value)}
                      >
                        <Reveal show={!draft.twoStep} className={styles.warning}>
                          <TriangleAlert size={14} strokeWidth={1.9} aria-hidden="true" />
                          Anyone with your password could sign in.
                        </Reveal>
                      </Row>
                      <Row
                        id={`${titleId}-alerts`}
                        label="Sign-in alerts"
                        description="Email you when a new device signs in."
                        checked={draft.alerts}
                        onChange={(value) => update("alerts", value)}
                      />
                    </Group>
                    <div className={styles.inlineField}>
                      <Select
                        label="Sign out idle sessions"
                        value={draft.timeout}
                        options={timeouts}
                        description="Applies to every device except this one."
                        onValueChange={(value) => update("timeout", value)}
                      />
                    </div>
                    <section className={styles.group} aria-labelledby={`${titleId}-devices`}>
                      <h4 id={`${titleId}-devices`} tabIndex={-1}>
                        Where you are signed in
                      </h4>
                      <ul className={styles.sessions}>
                        <AnimatePresence initial={false}>
                          {visibleSessions.map((session) => {
                            const Icon = session.icon;
                            return (
                              <motion.li
                                key={session.id}
                                className={styles.session}
                                initial={{ height: 0, opacity: 0 }}
                                animate={{ height: "auto", opacity: 1 }}
                                exit={{ height: 0, opacity: 0 }}
                                transition={reduced ? { duration: 0 } : { height: motionTokens.spring.smooth, opacity: { duration: motionTokens.duration.fast } }}
                              >
                                <div className={styles.sessionInner}>
                                  <Icon size={16} strokeWidth={1.75} aria-hidden="true" />
                                  <span className={styles.sessionText}>
                                    <strong>
                                      {session.device} · {session.app}
                                    </strong>
                                    <span>
                                      {session.place} · {session.seen}
                                    </span>
                                  </span>
                                  {session.current ? (
                                    <span className={styles.thisDevice}>This device</span>
                                  ) : (
                                    <Button
                                      id={`${titleId}-signout-${session.id}`}
                                      type="button"
                                      variant="ghost"
                                      size="sm"
                                      aria-label={`Sign out ${session.device}`}
                                      onClick={() => {
                                        const others = visibleSessions.filter((item) => !item.current);
                                        const index = others.findIndex((item) => item.id === session.id);
                                        const next = others[index + 1] ?? others[index - 1];
                                        update("signedOut", [...draft.signedOut, session.id]);
                                        setLiveMessage(`${session.device} will be signed out when you save`);
                                        focusById(next ? `${titleId}-signout-${next.id}` : `${titleId}-devices`);
                                      }}
                                    >
                                      Sign out
                                    </Button>
                                  )}
                                </div>
                              </motion.li>
                            );
                          })}
                        </AnimatePresence>
                      </ul>
                    </section>
                  </>
                ) : null}
                {tab === "billing" ? (
                  <>
                    <div className={styles.plan}>
                      <div className={styles.planText}>
                        <strong>Team plan</strong>
                        <span>
                          8 seats at{"\u00a0"}
                          <span className={styles.inherit}>
                            <AnimatedCounter value={draft.cycle === "yearly" ? 15 : 18} prefix="$" />
                          </span>
                          {"\u00a0each"}
                        </span>
                      </div>
                      <div className={styles.price}>
                        <span className={styles.priceValue}>
                          <AnimatedCounter value={price} prefix="$" />
                        </span>
                        <TextMorph className={styles.priceUnit}>{draft.cycle === "yearly" ? "per year" : "per month"}</TextMorph>
                      </div>
                    </div>
                    <div className={styles.cycle}>
                      <SegmentedControl label="Billing cycle" value={draft.cycle} options={cycles} onValueChange={(value) => update("cycle", value as SettingsState["cycle"])} />
                      <span className={styles.cycleNote}>
                        <AnimatePresence mode="popLayout" initial={false}>
                          <motion.span
                            key={draft.cycle}
                            initial={{ opacity: 0, y: 6, filter: blur(motionTokens.blur.soft) }}
                            animate={{ opacity: 1, y: 0, filter: blur(0) }}
                            exit={{ opacity: 0, y: -4, filter: blur(motionTokens.blur.subtle) }}
                            transition={{ duration: reduced ? 0 : motionTokens.duration.standard, ease: enterEase }}
                          >
                            {draft.cycle === "yearly" ? "You save $288 a year" : "Save $288 a year with yearly billing"}
                          </motion.span>
                        </AnimatePresence>
                      </span>
                    </div>
                    <div className={styles.payment}>
                      <CreditCard size={16} strokeWidth={1.75} aria-hidden="true" />
                      <span>
                        <strong>Visa ending in 4242</strong>
                        <span>Expires 08/2028 · next invoice on October 1</span>
                      </span>
                    </div>
                    <div className={styles.fields}>
                      <Input
                        id={`${titleId}-billingEmail`}
                        label="Billing email"
                        type="email"
                        value={draft.billingEmail}
                        autoComplete="email"
                        spellCheck={false}
                        description="Invoices and receipts are sent here."
                        className={shakeFor("billingEmail")}
                        error={errorFor("billingEmail")}
                        onBlur={() => reveal("billingEmail")}
                        onChange={(event) => update("billingEmail", event.target.value)}
                      />
                      <Input
                        id={`${titleId}-company`}
                        label="Company name"
                        value={draft.company}
                        autoComplete="organization"
                        className={shakeFor("company")}
                        onChange={(event) => update("company", event.target.value)}
                      />
                      <Select label="Country" value={draft.country} options={countries} onValueChange={(value) => update("country", value)} />
                      <Input
                        id={`${titleId}-vat`}
                        label="VAT number"
                        value={draft.vat}
                        spellCheck={false}
                        className={shakeFor("vat")}
                        onChange={(event) => update("vat", event.target.value)}
                      />
                    </div>
                  </>
                ) : null}
              </motion.div>
            </AnimatePresence>
          </div>
          <div className={styles.dock}>
            <AnimatePresence>
              {barState ? (
                <SaveBar key="bar" barRef={bar} state={barState} count={dirtyCount} shake={shakeX} onDiscard={discard} onSave={save} />
              ) : null}
            </AnimatePresence>
          </div>
          <p className={styles.srOnly} role="status" aria-live="polite">
            {liveMessage}
          </p>
        </div>
      </div>
    </section>
  );
}

export default SettingsPage;
