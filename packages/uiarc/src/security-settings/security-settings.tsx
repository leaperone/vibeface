"use client";
import { useCallback, useEffect, useId, useRef, useState, type ReactNode, type RefObject } from "react";
import { AnimatePresence, LayoutGroup, MotionConfig, motion, useReducedMotion, type Variants } from "motion/react";
import { Check, Download, Laptop, LogOut, Monitor, ShieldOff, Smartphone, Tablet } from "lucide-react";
import { Badge } from "../badge/badge";
import { Button } from "../button/button";
import { Checkbox } from "../checkbox/checkbox";
import { CopyButton } from "../copy-button/copy-button";
import { HoldToConfirm } from "../hold-to-confirm/hold-to-confirm";
import { OtpInput } from "../otp-input/otp-input";
import { PasswordField } from "../password-field/password-field";
import { PasswordStrength, estimateStrength } from "../password-strength/password-strength";
import { motionTokens } from "../motion-tokens";
import styles from "./security-settings.module.css";

const ACCOUNT = "maya@fieldnote.example";
const SETUP_KEY = "N4XG K3LQ 7RPM 2VWD Y6HT A5CJ";
const DEMO_CODE = "246810";
const BACKUP_CODES = ["7kq2-m9xp", "c4tn-8wzr", "p3hd-6yvb", "x9fm-2rkt", "b5wl-4qne", "r8jc-7tgs", "m2vx-9dph", "t6nz-3kwf", "h4gy-5bmq", "w7rs-8cjd"];

type Step = "off" | "scan" | "verify" | "codes" | "on";
type SaveState = "idle" | "saving" | "done";
type PasswordState = "idle" | "saving" | "saved";
type DeviceKind = "laptop" | "phone" | "desktop" | "tablet";

interface Session {
  id: string;
  device: string;
  client: string;
  city: string;
  seen: string;
  kind: DeviceKind;
  current?: boolean;
  flag?: string;
}

const STEP_ORDER: Record<Step, number> = { off: 0, scan: 1, verify: 2, codes: 3, on: 4 };
const RAIL = [
  { step: "scan", label: "Scan" },
  { step: "verify", label: "Verify" },
  { step: "codes", label: "Save codes" },
] as const;

const DEVICE_ICONS = { laptop: Laptop, phone: Smartphone, desktop: Monitor, tablet: Tablet } as const;

const SESSIONS: Session[] = [
  { id: "mbp", device: "MacBook Pro", client: "Chrome 129", city: "San Francisco, US", seen: "Active now", kind: "laptop", current: true },
  { id: "iphone", device: "iPhone 15 Pro", client: "Fieldnote for iOS", city: "San Francisco, US", seen: "2 hours ago", kind: "phone" },
  { id: "windows", device: "Windows desktop", client: "Edge 128", city: "Austin, US", seen: "Yesterday at 4:12 PM", kind: "desktop" },
  { id: "ipad", device: "iPad Air", client: "Safari 18", city: "Lisbon, PT", seen: "Sep 14", kind: "tablet" },
  { id: "thinkpad", device: "ThinkPad X1", client: "Firefox 130", city: "Berlin, DE", seen: "Sep 9", kind: "laptop", flag: "New location" },
];

/** The three finder squares of the QR code, as grid areas: row start / column start / row end / column end. */
const FINDERS = ["1 / 1 / 8 / 8", "1 / 19 / 8 / 26", "19 / 1 / 26 / 8"];

/**
 * A deterministic module pattern for the demo code: a seeded generator keeps server and client markup
 * identical, and the three finder corners plus the timing lines keep it reading as a QR code.
 */
const QR_MODULES = (() => {
  let seed = 0x5eed1e55;
  const random = () => {
    let t = (seed = (seed + 0x6d2b79f5) | 0);
    t = Math.imul(t ^ (t >>> 15), 1 | t);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 0x100000000;
  };
  const modules: { row: number; col: number }[] = [];
  for (let row = 0; row < 25; row++) {
    for (let col = 0; col < 25; col++) {
      if ((row < 8 && col < 8) || (row < 8 && col > 16) || (row > 16 && col < 8)) continue;
      const ring = Math.max(Math.abs(row - 18), Math.abs(col - 18));
      const inked =
        ring <= 2 ? ring !== 1 : row === 6 || col === 6 ? (row + col) % 2 === 0 : random() < 0.46;
      if (inked) modules.push({ row, col });
    }
  }
  return modules;
})();

const { spring, duration, blur, stagger } = motionTokens;
const easeEnter = [...motionTokens.ease.enter] as [number, number, number, number];
const easeStandard = [...motionTokens.ease.standard] as [number, number, number, number];
/** Reduced motion collapses every transition in this block to no time at all. */
const still = { duration: 0 } as const;

const slide: Variants = {
  enter: (direction: number) => ({ opacity: 0, x: 24 * direction, filter: `blur(${blur.soft}px)` }),
  center: { opacity: 1, x: 0, filter: "blur(0px)" },
  exit: (direction: number) => ({
    opacity: 0,
    x: -24 * direction,
    filter: `blur(${blur.soft}px)`,
    transition: { duration: duration.exit, ease: easeStandard },
  }),
};

const rise: Variants = {
  enter: (direction: number) => ({ opacity: 0, y: `${0.5 * direction}em`, filter: `blur(${blur.subtle}px)` }),
  center: { opacity: 1, y: "0em", filter: "blur(0px)" },
  exit: (direction: number) => ({
    opacity: 0,
    y: `${-0.5 * direction}em`,
    filter: `blur(${blur.subtle}px)`,
    transition: { duration: duration.instant, ease: easeStandard },
  }),
};

/** Follows the natural height of a region so an expand can animate to it without a guessed value. */
function useMeasuredHeight(): [RefObject<HTMLDivElement | null>, number | "auto"] {
  const ref = useRef<HTMLDivElement>(null);
  const [height, setHeight] = useState<number | "auto">("auto");
  useEffect(() => {
    const element = ref.current;
    if (!element || typeof ResizeObserver === "undefined") return;
    const observer = new ResizeObserver(() => setHeight(element.offsetHeight));
    observer.observe(element);
    return () => observer.disconnect();
  }, []);
  return [ref, height];
}

interface CollapseProps {
  children: ReactNode;
  className?: string;
}

/** Expanding a section is a height spring on a wrapper, so the content inside keeps its own layout. */
function Collapse({ children, className }: CollapseProps) {
  const reduce = useReducedMotion();
  const [inner, height] = useMeasuredHeight();
  return (
    <motion.div
      className={[styles.collapse, className].filter(Boolean).join(" ")}
      initial={{ height: 0, opacity: 0 }}
      animate={{ height, opacity: 1 }}
      exit={{
        height: 0,
        opacity: 0,
        transition: reduce ? still : { height: spring.smooth, opacity: { duration: duration.fast, ease: easeStandard } },
      }}
      transition={reduce ? still : { height: spring.smooth, opacity: { duration: duration.standard, ease: easeEnter } }}
    >
      <div ref={inner}>{children}</div>
    </motion.div>
  );
}

/** One line of copy that rises out and blurs while the next rises in; the plain sentence stays for screen readers. */
function SwapText({ text }: { text: string }) {
  const reduce = useReducedMotion();
  return (
    <span className={styles.swap}>
      <span className={styles.srOnly}>{text}</span>
      <span className={styles.swapTrack} aria-hidden="true">
        <AnimatePresence mode="popLayout" initial={false}>
          <motion.span
            key={text}
            className={styles.swapText}
            initial={{ opacity: 0, y: "0.45em", filter: `blur(${blur.soft}px)` }}
            animate={{ opacity: 1, y: "0em", filter: "blur(0px)" }}
            exit={{
              opacity: 0,
              y: "-0.35em",
              filter: `blur(${blur.subtle}px)`,
              transition: reduce ? still : { duration: duration.fast, ease: easeStandard },
            }}
            transition={reduce ? still : { duration: duration.standard, ease: easeEnter }}
          >
            {text}
          </motion.span>
        </AnimatePresence>
      </span>
    </span>
  );
}

/** A number that rolls up or down depending on which way the count moved. */
function RollNumber({ value }: { value: number }) {
  const reduce = useReducedMotion();
  const [shown, setShown] = useState({ value, direction: 1 });
  if (shown.value !== value) setShown({ value, direction: value > shown.value ? 1 : -1 });
  return (
    <span className={styles.roll}>
      <span className={styles.srOnly}>{value}</span>
      <span className={styles.swapTrack} aria-hidden="true">
        <AnimatePresence mode="popLayout" initial={false} custom={shown.direction}>
          <motion.span
            key={value}
            className={styles.swapText}
            custom={shown.direction}
            variants={rise}
            initial="enter"
            animate="center"
            exit="exit"
            transition={reduce ? still : { y: spring.snappy, opacity: { duration: duration.fast }, filter: { duration: duration.fast } }}
          >
            {value}
          </motion.span>
        </AnimatePresence>
      </span>
    </span>
  );
}

/** The code is drawn from real cells so it scales with the block and animates in module by module. */
function QrCode() {
  return (
    <div className={styles.qr} role="img" aria-label={`QR code that adds Fieldnote for ${ACCOUNT} to an authenticator app`}>
      {FINDERS.map((area, index) => (
        <span key={area} className={styles.finder} style={{ gridArea: area, animationDelay: `${50 * index}ms` }}>
          <span className={styles.finderDot} />
        </span>
      ))}
      {QR_MODULES.map(({ row, col }) => (
        <span
          key={`${row}-${col}`}
          className={styles.qrCell}
          style={{ gridArea: `${row + 1} / ${col + 1}`, animationDelay: `${60 + (row + col) * 7}ms` }}
        />
      ))}
    </div>
  );
}

/** A ring draws itself, then the tick inside it, as the confirmation that two-factor is now on. */
function DrawnCheck() {
  const reduce = useReducedMotion();
  return (
    <svg className={styles.drawn} width={40} height={40} viewBox="0 0 40 40" fill="none" aria-hidden="true">
      <motion.circle
        cx={20}
        cy={20}
        r={18.5}
        stroke="currentColor"
        strokeWidth={1.5}
        transform="rotate(-90 20 20)"
        initial={{ pathLength: 0 }}
        animate={{ pathLength: 1 }}
        transition={reduce ? still : { duration: duration.considered, ease: easeEnter }}
      />
      <motion.path
        d="M13 20.5 18 25.5 27.5 15"
        stroke="currentColor"
        strokeWidth={2}
        strokeLinecap="round"
        strokeLinejoin="round"
        initial={{ pathLength: 0 }}
        animate={{ pathLength: 1 }}
        transition={reduce ? still : { duration: duration.standard, ease: easeEnter, delay: 0.34 }}
      />
    </svg>
  );
}

/** A finished step swaps its number for a tick that draws itself. */
function RailMark({ index, done }: { index: number; done: boolean }) {
  const reduce = useReducedMotion();
  return (
    <span className={styles.railMark} aria-hidden="true">
      <AnimatePresence mode="popLayout" initial={false}>
        {done ? (
          <motion.svg
            key="done"
            width={12}
            height={12}
            viewBox="0 0 12 12"
            fill="none"
            initial={{ opacity: 0, scale: 0.6 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.6 }}
            transition={reduce ? still : spring.snappy}
          >
            <motion.path
              d="M2.5 6.25 5 8.75 9.5 3.5"
              stroke="currentColor"
              strokeWidth={1.75}
              strokeLinecap="round"
              strokeLinejoin="round"
              initial={{ pathLength: 0 }}
              animate={{ pathLength: 1 }}
              transition={reduce ? still : { duration: duration.standard, ease: easeEnter, delay: 0.06 }}
            />
          </motion.svg>
        ) : (
          <motion.span
            key="number"
            initial={{ opacity: 0, scale: 0.6 }}
            animate={{ opacity: 1, scale: 1 }}
            exit={{ opacity: 0, scale: 0.6 }}
            transition={reduce ? still : spring.snappy}
          >
            {index + 1}
          </motion.span>
        )}
      </AnimatePresence>
    </span>
  );
}

interface SessionRowProps {
  session: Session;
  onRevoke: () => void;
  register: (element: HTMLButtonElement | null) => void;
}

/** Each row owns its height so a revoke can fold the row away while the rows below slide up. */
function SessionRow({ session, onRevoke, register }: SessionRowProps) {
  const reduce = useReducedMotion();
  const [inner, height] = useMeasuredHeight();
  const DeviceIcon = DEVICE_ICONS[session.kind];
  const variants = {
    exit: (order: Record<string, number>) => {
      const delay = (order[session.id] ?? 0) * stagger.line;
      return reduce
        ? { height: 0, opacity: 0, transition: still }
        : {
            height: 0,
            opacity: 0,
            x: -12,
            transition: {
              height: { ...spring.smooth, delay },
              x: { ...spring.smooth, delay },
              opacity: { duration: duration.standard, ease: easeStandard, delay },
            },
          };
    },
  };
  return (
    <motion.li
      className={styles.session}
      variants={variants}
      initial={false}
      animate={{ height, opacity: 1, x: 0 }}
      exit="exit"
      transition={reduce ? still : { height: spring.smooth }}
    >
      <div ref={inner}>
        <div className={styles.sessionInner}>
          <DeviceIcon className={styles.deviceIcon} size={16} strokeWidth={1.5} aria-hidden="true" />
          <div className={styles.sessionText}>
            <span className={styles.sessionName}>
              {session.device}
              {session.current && (
                <Badge size="sm">This device</Badge>
              )}
              {session.flag && (
                <Badge size="sm" tone="warning">
                  {session.flag}
                </Badge>
              )}
            </span>
            <span className={styles.sessionMeta}>
              {session.client} · {session.city} · {session.seen}
            </span>
          </div>
          {!session.current && (
            <Button
              ref={register}
              type="button"
              variant="ghost"
              size="sm"
              aria-label={`Revoke ${session.device} session in ${session.city}`}
              onClick={onRevoke}
            >
              Revoke
            </Button>
          )}
        </div>
      </div>
    </motion.li>
  );
}

interface PasswordError {
  field: "current" | "new";
  text: string;
}

/**
 * Two-factor setup, password change and the signed-in device list, in one panel. Recovery codes and the
 * device list are held locally: the block is a complete demo, with the code 246810 standing in for an
 * authenticator app and the prorated upgrade-free copy kept exact so the flow can be tried end to end.
 */
export function SecuritySettings() {
  const blockId = useId();
  const reduce = useReducedMotion();

  const [step, setStep] = useState<Step>("off");
  const [direction, setDirection] = useState(1);
  const [code, setCode] = useState("");
  const [codeError, setCodeError] = useState("");
  const [checking, setChecking] = useState(false);
  const [codesSaved, setCodesSaved] = useState(false);
  const [saveState, setSaveState] = useState<SaveState>("idle");
  const [passwordOpen, setPasswordOpen] = useState(false);
  const [currentPassword, setCurrentPassword] = useState("");
  const [newPassword, setNewPassword] = useState("");
  const [signOutOthers, setSignOutOthers] = useState(false);
  const [passwordState, setPasswordState] = useState<PasswordState>("idle");
  const [passwordError, setPasswordError] = useState<PasswordError | null>(null);
  const [lastChanged, setLastChanged] = useState("Last changed March 4, 2026");
  const [sessions, setSessions] = useState<Session[]>(SESSIONS);
  const [exitOrder, setExitOrder] = useState<Record<string, number>>({});
  const [holding, setHolding] = useState(false);
  const [live, setLive] = useState("");

  const timers = useRef<number[]>([]);
  const verifyTimer = useRef<number | null>(null);
  const sessionsRef = useRef(sessions);
  const tfaButton = useRef<HTMLButtonElement>(null);
  const continueButton = useRef<HTMLButtonElement>(null);
  const codesHeading = useRef<HTMLHeadingElement>(null);
  const enabledText = useRef<HTMLParagraphElement>(null);
  const changeButton = useRef<HTMLButtonElement>(null);
  const currentField = useRef<HTMLInputElement>(null);
  const newField = useRef<HTMLInputElement>(null);
  const sessionsHeading = useRef<HTMLHeadingElement>(null);
  const revokeButtons = useRef(new Map<string, HTMLButtonElement>());

  useEffect(() => {
    const pending = timers.current;
    return () => pending.forEach(id => window.clearTimeout(id));
  }, []);

  useEffect(() => {
    sessionsRef.current = sessions;
  }, [sessions]);

  const otherSessions = sessions.filter(session => !session.current);
  const settingUp = step === "scan" || step === "verify" || step === "codes";
  const railIndex = STEP_ORDER[step] - 1;

  const later = useCallback((run: () => void, delay: number) => {
    const id = window.setTimeout(run, delay);
    timers.current.push(id);
    return id;
  }, []);

  /** Focus only after the new pane has painted, so a ref that is still null does not swallow it. */
  function focusLater(target: () => HTMLElement | null | undefined) {
    requestAnimationFrame(() =>
      requestAnimationFrame(() => {
        const element = target();
        if (element?.isConnected) element.focus({ preventScroll: true });
      }),
    );
  }

  function cancelVerify() {
    if (verifyTimer.current !== null) window.clearTimeout(verifyTimer.current);
    verifyTimer.current = null;
    setChecking(false);
  }

  function goTo(next: Step) {
    setDirection(STEP_ORDER[next] >= STEP_ORDER[step] ? 1 : -1);
    setStep(next);
  }

  function verify(value = code) {
    if (verifyTimer.current !== null) return;
    if (value.length < 6) {
      setCodeError("Enter all 6 digits from your app");
      return;
    }
    setChecking(true);
    verifyTimer.current = later(() => {
      verifyTimer.current = null;
      setChecking(false);
      if (value !== DEMO_CODE) {
        setCodeError("That code didn’t match. Try the newest code in your app");
      } else {
        goTo("codes");
        setLive("Code accepted. Save your backup codes to finish");
        focusLater(() => codesHeading.current);
      }
    }, 700);
  }

  /** Signs every other device out. Also runs right after a password change, which is why the copy differs. */
  function signOutAll(fromPassword = false) {
    const others = sessionsRef.current.filter(session => !session.current);
    if (!others.length) return;
    setExitOrder(Object.fromEntries(others.map((session, index) => [session.id, index])));
    setSessions(current => current.filter(session => session.current));
    setHolding(false);
    const count = `${others.length} other ${others.length === 1 ? "session" : "sessions"}`;
    setLive(fromPassword ? `Password updated and signed out of ${count}` : `Signed out of ${count}`);
    if (!fromPassword) focusLater(() => sessionsHeading.current);
  }

  function revoke(session: Session) {
    const index = otherSessions.findIndex(item => item.id === session.id);
    const neighbour = otherSessions[index + 1] ?? otherSessions[index - 1];
    setExitOrder({});
    setSessions(current => current.filter(item => item.id !== session.id));
    setLive(`Signed out of ${session.device} in ${session.city}`);
    focusLater(() => (neighbour ? revokeButtons.current.get(neighbour.id) : null) ?? sessionsHeading.current);
  }

  function submitPassword(event: React.FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (passwordState !== "idle") return;
    const fail = (field: PasswordError["field"], text: string) => {
      setPasswordError({ field, text });
      (field === "current" ? currentField : newField).current?.focus();
    };
    if (!currentPassword) return fail("current", "Enter your current password");
    if (!newPassword) return fail("new", "Choose a new password");
    if (newPassword === currentPassword) return fail("new", "Use a password that differs from your current one");
    if (estimateStrength(newPassword).level < 3) return fail("new", "Meet at least three of the requirements below");
    setPasswordError(null);
    setPasswordState("saving");
    later(() => {
      setPasswordState("saved");
      setLive("Password updated");
      if (signOutOthers) signOutAll(true);
      later(() => {
        setPasswordOpen(false);
        setPasswordState("idle");
        setCurrentPassword("");
        setNewPassword("");
        setSignOutOthers(false);
        setLastChanged("Last changed just now");
        focusLater(() => changeButton.current);
      }, 1400);
    }, 900);
  }

  const paneTransition = reduce
    ? still
    : { x: spring.smooth, opacity: { duration: duration.standard, ease: easeEnter }, filter: { duration: duration.standard, ease: easeEnter } };
  const actionFade = {
    initial: { opacity: 0, filter: `blur(${blur.subtle}px)` },
    animate: { opacity: 1, filter: "blur(0px)" },
    exit: { opacity: 0, filter: `blur(${blur.subtle}px)`, transition: reduce ? still : { duration: duration.fast, ease: easeStandard } },
    transition: reduce ? still : { duration: duration.standard, ease: easeEnter },
  };

  return (
    <MotionConfig reducedMotion="user">
      <LayoutGroup id={blockId}>
        <section className={styles.block} aria-labelledby={`${blockId}-title`}>
          <header className={styles.header}>
            <h2 id={`${blockId}-title`}>Security</h2>
            <p>Control how you sign in to Fieldnote as {ACCOUNT}.</p>
          </header>

          <section className={styles.section} aria-labelledby={`${blockId}-tfa`}>
            <div className={styles.sectionHead}>
              <div className={styles.sectionText}>
                <h3 id={`${blockId}-tfa`}>Two-factor authentication</h3>
                <p>Ask for a code from your authenticator app when you sign in on a new device.</p>
              </div>
              <div className={styles.aside}>
                <Badge size="sm" tone={step === "on" ? "success" : "neutral"}>
                  {step === "on" ? "On" : settingUp ? "Setting up" : "Off"}
                </Badge>
                <AnimatePresence mode="popLayout" initial={false}>
                  {(step === "off" || step === "scan" || step === "verify") && (
                    <motion.span key="tfa-action" className={styles.actionSlot} {...actionFade}>
                      <Button
                        ref={tfaButton}
                        type="button"
                        size="sm"
                        variant={step === "off" ? "primary" : "ghost"}
                        aria-expanded={step !== "off"}
                        onClick={() => {
                          if (step === "off") {
                            setCode("");
                            setCodeError("");
                            setCodesSaved(false);
                            setSaveState("idle");
                            goTo("scan");
                            setLive("Scan the code with your authenticator app to continue");
                          } else {
                            cancelVerify();
                            goTo("off");
                            setLive("Two-factor setup canceled");
                          }
                        }}
                      >
                        {step === "off" ? "Enable" : "Cancel"}
                      </Button>
                    </motion.span>
                  )}
                </AnimatePresence>
              </div>
            </div>

            <AnimatePresence initial={false}>
              {step !== "off" && (
                <Collapse key="tfa-body">
                  <div className={styles.tfaBody}>
                    <AnimatePresence initial={false}>
                      {settingUp && (
                        <Collapse key="rail">
                          <ol className={styles.rail} aria-label="Setup progress">
                            {RAIL.map((item, index) => {
                              const state = index < railIndex ? "done" : index === railIndex ? "current" : "next";
                              return (
                                <li
                                  key={item.step}
                                  className={styles.railItem}
                                  data-state={state}
                                  aria-current={state === "current" ? "step" : undefined}
                                >
                                  {state === "current" && (
                                    <motion.span layoutId="security-rail" className={styles.railHighlight} transition={reduce ? still : spring.morph} />
                                  )}
                                  <RailMark index={index} done={state === "done"} />
                                  <span className={styles.railLabel}>
                                    {item.label}
                                    {state === "done" && <span className={styles.srOnly}>, done</span>}
                                  </span>
                                </li>
                              );
                            })}
                          </ol>
                        </Collapse>
                      )}
                    </AnimatePresence>

                    <div className={styles.stage}>
                      <AnimatePresence mode="popLayout" initial={false} custom={direction}>
                        <motion.div
                          key={step}
                          className={styles.stagePane}
                          custom={direction}
                          variants={slide}
                          initial="enter"
                          animate="center"
                          exit="exit"
                          transition={paneTransition}
                        >
                          {step === "scan" && (
                            <div className={styles.scan}>
                              <QrCode />
                              <div className={styles.scanCopy}>
                                <div className={styles.copyBlock}>
                                  <p className={styles.lead}>Scan with your authenticator app</p>
                                  <p className={styles.muted}>
                                    1Password, Authy, and Google Authenticator all work. It adds Fieldnote for {ACCOUNT}.
                                  </p>
                                </div>
                                <div className={styles.copyBlock}>
                                  <p className={styles.keyLabel}>Can’t scan it? Enter this setup key</p>
                                  <div className={styles.keyRow}>
                                    <code className={styles.key}>{SETUP_KEY}</code>
                                    <CopyButton
                                      value={SETUP_KEY.replace(/\s/g, "")}
                                      label="Copy key"
                                      onCopied={() => setLive("Setup key copied")}
                                    />
                                  </div>
                                </div>
                                <div className={styles.stepActions}>
                                  <Button ref={continueButton} type="button" size="sm" onClick={() => goTo("verify")}>
                                    Continue
                                  </Button>
                                </div>
                              </div>
                            </div>
                          )}

                          {step === "verify" && (
                            <form
                              className={styles.verify}
                              noValidate
                              onSubmit={event => {
                                event.preventDefault();
                                verify();
                              }}
                            >
                              <OtpInput
                                label="Enter the 6-digit code from your app"
                                value={code}
                                onChange={value => {
                                  cancelVerify();
                                  setCode(value);
                                  setCodeError("");
                                  if (value.length === 6) verify(value);
                                }}
                                autoFocus
                                error={codeError || undefined}
                                description={checking ? "Checking the code" : `Demo code: ${DEMO_CODE}. Codes refresh every 30 seconds.`}
                              />
                              <div className={styles.stepActions}>
                                <Button
                                  type="button"
                                  variant="ghost"
                                  size="sm"
                                  onClick={() => {
                                    cancelVerify();
                                    setCodeError("");
                                    goTo("scan");
                                    focusLater(() => continueButton.current);
                                  }}
                                >
                                  Back
                                </Button>
                                <Button type="submit" size="sm" loading={checking}>
                                  Verify
                                </Button>
                              </div>
                            </form>
                          )}

                          {step === "codes" && (
                            <div className={styles.codesStep}>
                              <div className={styles.codesHead}>
                                <div className={styles.copyBlock}>
                                  <h4 ref={codesHeading} tabIndex={-1} className={styles.lead}>
                                    Save your backup codes
                                  </h4>
                                  <p className={styles.muted}>
                                    Each code signs you in once if you lose your phone. This is the only time they are shown.
                                  </p>
                                </div>
                                <div className={styles.codeActions}>
                                  <CopyButton
                                    value={[`Fieldnote backup codes for ${ACCOUNT}`, "", ...BACKUP_CODES].join("\n")}
                                    label="Copy codes"
                                    onCopied={() => {
                                      setCodesSaved(true);
                                      setLive("Backup codes copied");
                                    }}
                                  />
                                  <Button
                                    type="button"
                                    variant="secondary"
                                    size="sm"
                                    loading={saveState === "saving"}
                                    onClick={() => {
                                      if (saveState !== "idle") return;
                                      setSaveState("saving");
                                      later(() => {
                                        setSaveState("done");
                                        setCodesSaved(true);
                                        setLive("Saved fieldnote-backup-codes.txt");
                                      }, 600);
                                    }}
                                  >
                                    {saveState === "done" ? (
                                      <>
                                        <Check size={16} strokeWidth={2} aria-hidden="true" />
                                        Downloaded
                                      </>
                                    ) : (
                                      <>
                                        <Download size={16} strokeWidth={1.75} aria-hidden="true" />
                                        Download
                                      </>
                                    )}
                                  </Button>
                                </div>
                              </div>
                              <ol className={styles.codes} aria-label="Backup codes">
                                {BACKUP_CODES.map((value, index) => (
                                  <motion.li
                                    key={value}
                                    className={styles.code}
                                    initial={{ opacity: 0, y: 6, filter: `blur(${blur.soft}px)` }}
                                    animate={{ opacity: 1, y: 0, filter: "blur(0px)" }}
                                    transition={
                                      reduce ? still : { duration: duration.standard, ease: easeEnter, delay: 0.14 + index * stagger.item }
                                    }
                                  >
                                    {value}
                                  </motion.li>
                                ))}
                              </ol>
                              <div className={styles.codesFoot}>
                                <p className={styles.muted}>
                                  <SwapText text={codesSaved ? "Codes saved. Finish to turn on two-factor" : "Copy or download the codes to finish"} />
                                </p>
                                <Button
                                  type="button"
                                  size="sm"
                                  disabled={!codesSaved}
                                  onClick={() => {
                                    goTo("on");
                                    setLive("Two-factor authentication is on");
                                    focusLater(() => enabledText.current);
                                  }}
                                >
                                  Finish setup
                                </Button>
                              </div>
                            </div>
                          )}

                          {step === "on" && (
                            <div className={styles.enabled}>
                              <span className={styles.enabledMark}>
                                <DrawnCheck />
                              </span>
                              <div className={styles.enabledText}>
                                <p ref={enabledText} tabIndex={-1} className={styles.lead}>
                                  Two-factor authentication is on
                                </p>
                                <p className={styles.muted}>
                                  Authenticator app added Sep 22, 2026 · <span className={styles.num}>10</span> of{" "}
                                  <span className={styles.num}>10</span> backup codes unused
                                </p>
                              </div>
                              <HoldToConfirm
                                className={styles.hold}
                                label="Hold to turn off"
                                confirmedLabel="Turned off"
                                icon={<ShieldOff size={16} strokeWidth={1.75} aria-hidden="true" />}
                                onConfirm={() => {
                                  later(() => {
                                    goTo("off");
                                    setLive("Two-factor authentication turned off");
                                    focusLater(() => tfaButton.current);
                                  }, 520);
                                }}
                              />
                            </div>
                          )}
                        </motion.div>
                      </AnimatePresence>
                    </div>
                  </div>
                </Collapse>
              )}
            </AnimatePresence>
          </section>

          <section className={styles.section} aria-labelledby={`${blockId}-password`}>
            <div className={styles.sectionHead}>
              <div className={styles.sectionText}>
                <h3 id={`${blockId}-password`}>Password</h3>
                <p>
                  <SwapText text={lastChanged} />
                </p>
              </div>
              <div className={styles.aside}>
                <Button
                  ref={changeButton}
                  type="button"
                  size="sm"
                  variant={passwordOpen ? "ghost" : "secondary"}
                  disabled={passwordState !== "idle"}
                  aria-expanded={passwordOpen}
                  onClick={() => {
                    if (passwordState !== "idle") return;
                    if (passwordOpen) {
                      setPasswordOpen(false);
                      setPasswordError(null);
                      setLive("Password change canceled");
                      return;
                    }
                    setPasswordOpen(true);
                    focusLater(() => currentField.current);
                  }}
                >
                  {passwordOpen ? "Cancel" : "Change password"}
                </Button>
              </div>
            </div>
            <AnimatePresence initial={false}>
              {passwordOpen && (
                <Collapse key="password-form">
                  <form className={styles.pwForm} noValidate onSubmit={submitPassword}>
                    <div>
                      <PasswordField
                        ref={currentField}
                        label="Current password"
                        autoComplete="current-password"
                        value={currentPassword}
                        readOnly={passwordState !== "idle"}
                        aria-invalid={passwordError?.field === "current" || undefined}
                        aria-describedby={passwordError?.field === "current" ? `${blockId}-current-error` : undefined}
                        onChange={event => {
                          setCurrentPassword(event.target.value);
                          if (passwordError?.field === "current") setPasswordError(null);
                        }}
                      />
                      <AnimatePresence initial={false}>
                        {passwordError?.field === "current" && (
                          <Collapse key="current-error">
                            <p id={`${blockId}-current-error`} className={styles.fieldError} role="alert">
                              {passwordError.text}
                            </p>
                          </Collapse>
                        )}
                      </AnimatePresence>
                    </div>
                    <PasswordStrength
                      ref={newField}
                      label="New password"
                      value={newPassword}
                      readOnly={passwordState !== "idle"}
                      error={passwordError?.field === "new" ? passwordError.text : undefined}
                      onValueChange={value => {
                        setNewPassword(value);
                        if (passwordError?.field === "new") setPasswordError(null);
                      }}
                    />
                    <div className={styles.pwFoot}>
                      <span className={styles.checkWrap}>
                        <Checkbox
                          label="Sign out of other sessions"
                          checked={signOutOthers}
                          disabled={!otherSessions.length || passwordState !== "idle"}
                          onCheckedChange={value => setSignOutOthers(value === true)}
                        />
                      </span>
                      <Button type="submit" size="sm" loading={passwordState === "saving"}>
                        {passwordState === "saved" ? (
                          <>
                            <Check size={16} strokeWidth={2} aria-hidden="true" />
                            Password updated
                          </>
                        ) : (
                          "Update password"
                        )}
                      </Button>
                    </div>
                  </form>
                </Collapse>
              )}
            </AnimatePresence>
          </section>

          <section className={styles.section} aria-labelledby={`${blockId}-sessions`}>
            <div className={styles.sectionHead}>
              <div className={styles.sectionText}>
                <h3 id={`${blockId}-sessions`} ref={sessionsHeading} tabIndex={-1}>
                  {"Active sessions "}
                  <span className={styles.count}>
                    <RollNumber value={sessions.length} />
                  </span>
                </h3>
                <p>Devices signed in to your account in the last 30 days.</p>
              </div>
            </div>
            <ul className={styles.sessions} aria-labelledby={`${blockId}-sessions`}>
              <AnimatePresence initial={false} custom={exitOrder}>
                {sessions.map(session => (
                  <SessionRow
                    key={session.id}
                    session={session}
                    onRevoke={() => revoke(session)}
                    register={element => {
                      if (element) revokeButtons.current.set(session.id, element);
                      else revokeButtons.current.delete(session.id);
                    }}
                  />
                ))}
              </AnimatePresence>
            </ul>
            <AnimatePresence initial={false}>
              {otherSessions.length ? (
                <Collapse key="sign-out-all">
                  <div className={styles.sessionsFoot}>
                    <p className={styles.muted}>
                      <SwapText
                        text={holding ? `Keep holding to sign out ${otherSessions.length} ${otherSessions.length === 1 ? "device" : "devices"}` : "You stay signed in on this device"}
                      />
                    </p>
                    <HoldToConfirm
                      className={styles.hold}
                      label="Sign out of other sessions"
                      confirmedLabel="Signed out"
                      icon={<LogOut size={16} strokeWidth={1.75} aria-hidden="true" />}
                      onHoldChange={setHolding}
                      onConfirm={() => signOutAll()}
                    />
                  </div>
                </Collapse>
              ) : (
                <Collapse key="alone">
                  <p className={styles.alone}>Only this device is signed in.</p>
                </Collapse>
              )}
            </AnimatePresence>
          </section>

          <p className={styles.srOnly} aria-live="polite">
            {live}
          </p>
        </section>
      </LayoutGroup>
    </MotionConfig>
  );
}

export default SecuritySettings;
