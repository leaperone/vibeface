---
name: Vibeface Web App
description: A focused workspace for conversations, tasks, devices, and recovery.
colors:
  background-light: "oklch(1 0 0)"
  background-dark: "oklch(0.175 0.006 270)"
  foreground-light: "oklch(0.21 0.012 270)"
  foreground-dark: "oklch(0.955 0.004 270)"
  brand-light: "oklch(0.53 0.2 280)"
  brand-dark: "oklch(0.72 0.15 280)"
  muted-light: "oklch(0.965 0.004 270)"
  muted-dark: "oklch(0.245 0.008 270)"
  danger-light: "oklch(0.54 0.2 27)"
  danger-dark: "oklch(0.72 0.17 25)"
typography:
  body:
    fontFamily: "Geist Variable, -apple-system, BlinkMacSystemFont, system-ui, sans-serif"
    fontSize: "15px"
    lineHeight: "24px"
  meta:
    fontFamily: "Geist Variable, -apple-system, BlinkMacSystemFont, system-ui, sans-serif"
    fontSize: "13px"
    lineHeight: "20px"
rounded:
  base: "0.625rem"
  xl: "0.875rem"
spacing:
  unit: "4px"
  page: "16px"
components:
  button-primary:
    backgroundColor: "{colors.brand-light}"
    rounded: "{rounded.base}"
  card:
    backgroundColor: "{colors.background-light}"
    rounded: "{rounded.xl}"
---

# Design System: Vibeface Web App

## Overview

**Creative North Star: "Quiet control room"**

The web app is an Operate surface. It keeps attention on the current conversation or task while the sidebar provides stable orientation. Geist, cool neutral surfaces, and a restrained violet brand accent make state changes legible without turning the workspace into a dashboard of competing signals.

**Key Characteristics:** calm, task-first, state-explicit, keyboard-friendly, responsive.

## Colors

The shared source of truth is `packages/theme/theme.css`. Light mode uses white surfaces and cool near-black text; dark mode separates page, card, popover, and sidebar by tonal steps. Violet is reserved for focus, selection, brand identity, and primary action. Success, warning, danger, and info colors communicate state and must be paired with text or icon labels.

## Typography

Geist Variable is the UI voice. The 15/24 body scale supports dense task reading; 13/20 metadata recedes without becoming inaccessible. Headings use tight tracking and balanced wrapping. Geist Mono is reserved for code, IDs, logs, and measurements.

## Layout

The workspace uses a persistent sidebar at wide widths and a single content column capped by `--page-width` (48rem). Header, page content, and task panels share the same alignment. Mobile collapses navigation and preserves 44px touch targets. Use `gap` and the existing 4px rhythm; keep generous separation between sections and tighter spacing within a control group.

## Elevation & Depth

Depth is tonal first. Light mode uses subtle `elevation-xs`/`elevation-sm` on interactive cards; dark mode uses surface contrast and outlines, reserving a larger shadow for floating layers. Do not stack a border and a broad shadow on ordinary cards.

## Shapes

The base radius is 10px, with 14px cards and 20px sheets where the component already establishes that language. Pills are reserved for compact status controls. Focus is an inset or 2px ring derived from `--ring`.

## Components

- **Sidebar and navigation rows:** stable 40px desktop rows, 44px mobile rows, active state plus icon/label.
- **Interactive rows:** shared `interactive-row` utility for hover, focus, and pressed feedback.
- **Cards and list groups:** `bg-card`, border, and small elevation; list groups use dividers rather than nested cards.
- **Buttons and fields:** existing UI primitives provide variants, focus treatment, and mobile sizing.
- **EmptyState:** icon, concise heading, optional explanation, and one recovery action.
- **BotFace and status marks:** identity and state are reinforced by text or icons; color is never the only signal.

## Do's and Don'ts

- Do keep the primary task visible in the middle column.
- Do make recovery, waiting, and offline states explicit in words.
- Do reuse `packages/theme/theme.css` and the existing UI primitives.
- Do preserve keyboard order and 44px mobile hit areas.
- Don't add dashboard-style metric cards to task surfaces.
- Don't introduce gradients, decorative blur, or new color families without a product decision.
- Don't use color alone for status.
- Don't create one-off spacing or component variants when an existing primitive fits.

## Current review notes

- **Critique:** hierarchy is strongest in the workspace shell and conversation view; secondary settings and inbox rows should retain the same heading-to-metadata contrast.
- **Audit:** the source shows explicit focus, reduced-motion, safe-area, overflow, and touch-target handling. Rendered browser/device checks are still required for final acceptance.
- **Distill:** keep sidebar, header, current task/conversation, and one clear recovery action; remove decorative wrappers before adding new containers.
- **Layout:** preserve the single reading path: navigation → current context → task content → action. Use grouping and proximity before extra cards.
- **Optimize:** no source evidence justifies speculative memoization or virtualization. Measure route load, interaction latency, and long-list behavior before changing runtime code.
