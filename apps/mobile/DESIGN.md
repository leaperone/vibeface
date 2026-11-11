---
name: Vibeface Mobile App
description: A native workspace for conversations, tasks, devices, and recovery.
colors:
  background-light: "oklch(1 0 0)"
  background-dark: "oklch(0.175 0.006 270)"
  foreground-light: "oklch(0.21 0.012 270)"
  foreground-dark: "oklch(0.955 0.004 270)"
  primary-light: "oklch(0.24 0.016 270)"
  primary-foreground-light: "oklch(0.985 0.002 270)"
  primary-dark: "oklch(0.94 0.005 270)"
  primary-foreground-dark: "oklch(0.2 0.01 270)"
  brand-light: "oklch(0.53 0.2 280)"
  brand-dark: "oklch(0.72 0.15 280)"
  muted-light: "oklch(0.965 0.004 270)"
  muted-dark: "oklch(0.245 0.008 270)"
typography:
  body:
    fontFamily: "Geist"
    fontSize: "16px"
    lineHeight: "24px"
  meta:
    fontFamily: "Geist"
    fontSize: "13px"
    lineHeight: "18px"
rounded:
  base: "0.625rem"
  control: "0.5rem"
  xl: "0.875rem"
spacing:
  unit: "4px"
  screen: "16px"
components:
  button-primary:
    backgroundColor: "{colors.primary-light}"
    textColor: "{colors.primary-foreground-light}"
    rounded: "{rounded.control}"
  card:
    backgroundColor: "{colors.background-light}"
    rounded: "{rounded.xl}"
---

# Design System: Vibeface Mobile App

## Overview

**Creative North Star: "Quiet control room"**

Working description of the incumbent implementation, pending qualitative confirmation.

The mobile app is an Operate surface designed around one-handed progress: choose a Bot, read the current work, and act without losing context. Shared cool neutrals and violet brand accents keep the native shell calm; platform materials appear only where the OS supports them and are disabled when reduced transparency is enabled.

**Key Characteristics:** native, calm, task-first, accessible, interruption-aware.

## Colors

Mobile consumes the same semantic tokens from `packages/theme/theme.css` through Uniwind. Default buttons use the neutral `primary`/`primary-foreground` pair. Violet marks brand and focus; the chat send control may use the current Bot tint. Muted surfaces group content; success, warning, danger, and info states remain text-and-icon backed.

## Typography

Geist is used for UI text and Geist Mono for code, IDs, and logs. The existing 16/24 body and 13/18 metadata scale favors readable mobile density. Keep headings short enough to wrap without hiding the action that follows.

## Layout

Screens use safe-area insets, a centered max-width on wide windows, and a single vertical reading path on phones. Chat keeps the composer attached to the bottom while messages scroll independently. Use 44px minimum controls and preserve native back, keyboard, and modal focus behavior.

## Elevation & Depth

Prefer tonal grouping and native sheets. `ChatMaterial` may use iOS Liquid Glass when available and falls back to a solid bordered surface; reduced transparency always wins. Avoid decorative blur or stacked shadows.

## Shapes

Rounded 12–14px cards and 24–26px chat materials establish the current language. Pills are for compact status or attachment tokens. Borders and active fills must remain visible in both themes.

## Components

- **Button/Input/Text primitives:** shared Uniwind variants with accessible labels and selected/disabled states.
- **Chat bubbles and composer:** distinct sender grouping, attachment controls, stop/send actions, and keyboard-safe layout.
- **Home list and workspace rows:** compact identity plus state, with clear press feedback.
- **Sheets and modals:** protected focus for target selection, recovery, and queue actions.
- **EmptyState and ErrorText:** a concise explanation and one retry or recovery action.

## Do's and Don'ts

- Do honor safe areas, dynamic type constraints, reduced motion, and reduced transparency.
- Do keep destructive or uncertain operations explicit and recoverable.
- Do use the shared semantic theme instead of literal per-screen colors.
- Do keep touch targets at least 44px even when the icon is smaller.
- Don't replace native navigation or sheets with web-style scaffolding.
- Don't use blur, gradients, or animation as decoration.
- Don't rely on color alone for connection, delivery, or recovery state.
- Don't add a second mobile design language beside the existing UI primitives.
