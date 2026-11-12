# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

Individuals managing their own Bot conversations, tasks, devices, and recovery state.

## Product Purpose

Vibeface gives one person a safe workspace for directing Bot work, reviewing outcomes, and keeping connected devices under personal control. Success means the person can understand what is waiting, act on it, and recover from uncertain delivery without repeating a side effect.

## Positioning

The workspace separates Bot conversations from device identity and delivery state while keeping their causal context visible.

## Operating Context

People use the web app as a desktop or mobile browser workspace. The sidebar, inbox, task views, device views, and conversation screens are the primary operating surfaces.

## Capabilities and Constraints

The app supports Bot conversations, tasks, channels, inbox decisions, device joining, recovery, settings, and local/offline state. Account, device, event, attachment, route, and command data stays within the person's network. Unknown write results must be reconciled before retrying.

## Brand Commitments

The product name is Vibeface. Existing Bot faces and the shared cool-neutral/violet theme are retained.

## Evidence on Hand

Routes and components under `apps/app/src/`, shared semantic tokens in `packages/theme/theme.css`, and existing accessibility and reduced-motion handling.

## Product Principles

- Make the current task and its state easy to understand.
- Keep personal network and device boundaries explicit.
- Never imply a write succeeded when its result is unknown.
- Prefer one clear recovery action over decorative complexity.

## Accessibility & Inclusion

Keyboard focus, reduced motion, safe areas, readable metadata, visible labels, and touch-sized controls are product requirements.
