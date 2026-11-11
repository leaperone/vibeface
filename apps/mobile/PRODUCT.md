# Product

<!-- impeccable:product-schema 1 -->

## Platform

adaptive

## Users

Individuals managing their own Bot conversations, tasks, devices, and recovery state on iOS and Android.

## Product Purpose

Vibeface gives one person a safe native workspace for directing Bot work, reviewing outcomes, and keeping connected devices under personal control. Success means the person can understand what is waiting, act on it, and recover from uncertain delivery without repeating a side effect.

## Positioning

The native app keeps Bot conversations, device identity, and delivery state connected while honoring platform navigation, safe areas, keyboard behavior, and system accessibility settings.

## Operating Context

People use the mobile app one-handed and while switching between a Bot conversation, task details, device setup, and recovery. The home list, chat composer, workspace screens, sheets, and native navigation are the primary operating surfaces.

## Capabilities and Constraints

The app supports Bot conversations, tasks, channels, device joining, recovery, settings, attachments, and local/offline state. Account, device, event, attachment, route, and command data stays within the person's network. Unknown write results must be reconciled before retrying. iOS safe-area and reduced-transparency behavior, Android insets and system Back, and 44px/48dp touch targets must remain intact.

## Brand Commitments

The product name is Vibeface. Existing Bot faces and the shared cool-neutral/violet theme are retained.

## Evidence on Hand

Routes and components under `apps/mobile/`, shared semantic tokens in `packages/theme/theme.css`, and existing accessibility, reduced-motion, reduced-transparency, safe-area, and keyboard-aware handling.

## Product Principles

- Make the current task and its state easy to understand.
- Keep personal network and device boundaries explicit.
- Never imply a write succeeded when its result is unknown.
- Prefer one clear recovery action over decorative complexity.
- Honor each platform's native navigation and accessibility behavior.

## Accessibility & Inclusion

Dynamic text, screen-reader labels, reduced motion, reduced transparency, safe areas, readable metadata, system Back/edge-swipe, and 44px/48dp touch targets are product requirements.
