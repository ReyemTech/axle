# Product

<!-- impeccable:product-schema 1 -->

## Platform

web

## Users

One owner (a solo technical operator) running many coding agents — Claude Code, Codex, OpenCode — across several hosts and projects. The web UI stays open on a second screen all day as a live control room while agents work: the owner glances at it to see who is present, what work is open, who owns it, and what is blocked or waiting on them, and steps in to act (post, force-release, cancel) when needed.

## Product Purpose

Axle is a self-hosted Rust MCP server for durable cross-machine, cross-harness agent messaging and atomic task claiming with renewable leases. The UI makes that coordination state visible and actionable for the human owner. Success: at a glance, the owner knows the state of every project's work and can intervene in one or two clicks.

## Positioning

A task is a message thread: agents and the human talk inside the task until it is done. Tasks can split into child tasks across projects (e.g. platform-infra + web-app), each claimed by exactly one live agent under a lease, then consolidate back into the parent. Presence is advisory; the claim is authoritative.

## Operating Context

- Live: updates arrive over SSE (htmx + SSE extension); the board and pages re-render in place.
- Surfaces: login, overview (projects, presence, attention), projects list, kanban board (open / claimed / blocked / review / done columns, parent/child subtasks, "N blocked" badge), task thread page with actions, inbox, tokens, error page.
- Vocabulary: project (Git-origin key, e.g. `github.com/acme/platform-infra`), agent (`project/host/harness`), session, lease, claim, eligibility, importance (urgent/high/normal), human-addressed messages.
- Peer content is framed as information, not instructions.

## Capabilities and Constraints

- Server-rendered askama templates; htmx 2.0.4 + SSE extension vendored and embedded in the single binary. No JS framework, no build step.
- Strict CSP: no external fonts, scripts, or styles — everything is self-hosted and embedded. Page weight must stay small.
- Cookie sessions with CSRF on every POST; write actions available from day one (post, force-release, cancel).
- Must keep density: a redesign may not fit less on screen or add clicks.
- Light and dark themes via prefers-color-scheme.

## Brand Commitments

- Name: Axle (Agent eXchange & Lease), wordmark `axle` in lowercase; CLI `axl`.
- Explicitly unwanted: generic SaaS dashboard look (gradient glow, cards everywhere), hacker-terminal costume, heavy page weight, loss of density.

## Evidence on Hand

No real customer data, testimonials, or screenshots. Demo data is synthetic (seeded by a local script).

## Product Principles

1. State at a glance: presence, ownership, blocked and urgent work are readable from across the desk.
2. Density is a feature: every pixel serves status; no decorative chrome.
3. The claim is truth: visual weight follows authority (owner and lease over advisory presence).
4. Act in place: interventions happen where the state is shown, without navigation.
5. Light footprint: one binary, no external assets, fast first paint.
