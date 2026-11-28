---
name: axle
description: Block signalling control room for a fleet of coding agents; state is read from signal aspects, track rules and labelled cards.
colors:
  ground: "#edebe4"
  surface: "#f7f6f2"
  surface-2: "#e2dfd6"
  band: "#e2dfd6"
  card: "#fbfaf7"
  card-border: "#cfcbc0"
  ink: "#1e2124"
  muted: "#5a5e63"
  rule: "#c9c5ba"
  rule-strong: "#9e998c"
  focus: "#1f5fd1"
  selection: "#c8d6f2"
  danger: "#b11a2e"
  danger-soft: "#f6dcdc"
  danger-border: "#e3a5ac"
  ok: "#1b6e40"
  warn: "#835400"
  warn-soft: "#f3e6c8"
  sig-red: "#d7263d"
  sig-green: "#258a52"
  sig-amber: "#b07800"
  bar: "#1e2124"
  bar-text: "#edebe4"
  bar-muted: "#a9aaa6"
  bar-field: "#2b2f33"
  bar-rule: "#42474c"
  line-1: "#2d63c8"
  line-2: "#7b4cc2"
  line-3: "#0b7b7b"
  line-4: "#9a5b22"
  line-5: "#b23c88"
  line-6: "#4e6478"
  ground-dark: "#16181a"
  surface-dark: "#1c1f22"
  surface-2-dark: "#24282c"
  band-dark: "#1d2023"
  card-dark: "#272b2f"
  card-border-dark: "#383d42"
  ink-dark: "#e6e4dd"
  muted-dark: "#9da0a4"
  rule-dark: "#33373c"
  rule-strong-dark: "#50565c"
  focus-dark: "#7aa7ff"
  selection-dark: "#2f4a7a"
  danger-dark: "#ff6b78"
  danger-soft-dark: "#2e1d20"
  danger-border-dark: "#6a2b33"
  ok-dark: "#4cc487"
  warn-dark: "#f0a202"
  warn-soft-dark: "#2c2516"
  sig-red-dark: "#e8475a"
  sig-green-dark: "#3bb273"
  sig-amber-dark: "#f0a202"
  bar-dark: "#0e1011"
  line-1-dark: "#6e9bf0"
  line-2-dark: "#a884e8"
  line-3-dark: "#3dbdb8"
  line-4-dark: "#d08b4e"
  line-5-dark: "#e07ab9"
  line-6-dark: "#8fa3b6"
typography:
  page-title:
    fontFamily: "system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "22px"
    fontWeight: 600
    lineHeight: 1.25
    letterSpacing: "-0.01em"
  page-title-list:
    fontFamily: "system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "20px"
    fontWeight: 600
    lineHeight: 1.45
  headline:
    fontFamily: "system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "18px"
    fontWeight: 600
    lineHeight: 1.3
  emphasis:
    fontFamily: "system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "15px"
    fontWeight: 700
    lineHeight: 1.45
  title:
    fontFamily: "system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "14px"
    fontWeight: 600
    lineHeight: 1.35
  body:
    fontFamily: "system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "14px"
    fontWeight: 400
    lineHeight: 1.45
  meta:
    fontFamily: "system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "12.5px"
    fontWeight: 400
    lineHeight: 1.45
  label:
    fontFamily: "system-ui, -apple-system, Segoe UI, Roboto, sans-serif"
    fontSize: "12.5px"
    fontWeight: 700
    letterSpacing: "0.05em"
  mono:
    fontFamily: "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace"
    fontSize: "12px"
    fontWeight: 400
    lineHeight: 1.45
rounded:
  hairline: "1px"
  sm: "3px"
  md: "4px"
  lg: "6px"
  full: "50%"
spacing:
  xs: "4px"
  sm: "8px"
  md: "12px"
  lg: "16px"
  xl: "24px"
components:
  card:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.lg}"
    padding: "10px 12px"
  button-key:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "0 12px"
    height: "30px"
  card-blocked:
    backgroundColor: "{colors.danger-soft}"
    textColor: "{colors.ink}"
    rounded: "{rounded.lg}"
    padding: "10px 12px"
  column:
    backgroundColor: "{colors.band}"
    textColor: "{colors.ink}"
    rounded: "{rounded.lg}"
  column-head:
    height: "40px"
    padding: "0 12px"
    typography: "{typography.label}"
  button-primary:
    backgroundColor: "{colors.ink}"
    textColor: "{colors.ground}"
    rounded: "{rounded.md}"
    padding: "0 12px"
    height: "30px"
  button-primary-hover:
    backgroundColor: "{colors.muted}"
  button-secondary:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.md}"
    padding: "0 12px"
    height: "30px"
  button-danger:
    backgroundColor: "{colors.danger-soft}"
    textColor: "{colors.danger}"
    rounded: "{rounded.md}"
    padding: "0 12px"
    height: "30px"
  input:
    backgroundColor: "{colors.surface}"
    textColor: "{colors.ink}"
    rounded: "{rounded.sm}"
    padding: "5px 9px"
    height: "30px"
  topbar:
    backgroundColor: "{colors.bar}"
    textColor: "{colors.bar-text}"
    height: "44px"
    padding: "0 16px"
  panel:
    backgroundColor: "{colors.card}"
    textColor: "{colors.ink}"
    rounded: "{rounded.lg}"
    padding: "12px 14px 14px"
---

# Design System: axle

## Overview

**Creative North Star: "Block Signalling"**

Axle reads like a railway signal box. Every project is a line with its own colour. Every task is a block on that line. The lease on a claim is the occupancy of the block, and a small set of signal aspects (ring, half, square, centred ring, filled) says what state each block is in. It is a control room kept on a second screen all day, so the system favours state that can be read from across the desk over anything decorative.

The first build was pure hairlines and read as too minimal and hard to scan. The shipped system keeps the signalling grammar but adds structure: columns are tinted bands, tasks are bordered cards, actions are labelled buttons, and every glyph has a text label beside it. These additions are deliberate rules, not drift. Density is still protected: cards are compact (10px/12px padding, 12.5px meta) and no surface carries chrome that does not report state.

**Key Characteristics:**
- Neutral warm-paper ground and graphite ink; saturated colour is reserved for state and for project lines.
- Three signal hues (red, green, amber), each with a text-safe twin for type.
- Flat: no shadows. Depth is tonal (ground, band, card) plus 1px borders.
- Tabular numerals everywhere; monospace for agent ids, leases and keys.
- System font stack only (strict CSP, no external assets).
- Light and dark themes via `prefers-color-scheme`, same roles in both.

## Colors

A warm neutral field with graphite ink, three signal hues that mean state, and six project line colours that mean identity.

### Primary
- **Track Ink** (#1e2124 light, #e6e4dd dark): body text, primary buttons, the active tab underline, column header rules, focus-adjacent selection outline. It is the accent; there is no brand hue.

### Secondary (signal aspects, state only)
- **Occupied Red** (#d7263d light, #e8475a dark): claimed, blocked and urgent glyphs, the occupancy bar of a lease, the brand mark's square.
- **Clear Green** (#258a52 light, #3bb273 dark): done and live glyphs.
- **Caution Amber** (#b07800 light, #f0a202 dark): review, high importance, stale presence, low lease.
- **Text-safe twins**: Danger (#b11a2e / #ff6b78), OK (#1b6e40 / #4cc487) and Warn (#835400 / #f0a202) carry the same meaning in text, because the glyph hues fail contrast as type on the light ground.

### Tertiary (project lines)
- Six line hues, `line-1` to `line-6` (blue #2d63c8, violet #7b4cc2, teal #0b7b7b, ochre #9a5b22, magenta #b23c88, slate #4e6478), each lightened in dark mode. A neutral `line-0` (#9e998c) is the fallback. A project keeps one line colour everywhere.

### Neutral
- **Ground** (#edebe4 / #16181a): page background.
- **Band** (#e2dfd6 / #1d2023): column background; also hover wash.
- **Card** (#fbfaf7 / #272b2f) with **Card Border** (#cfcbc0 / #383d42): task cards, panels, counters, reader, lists.
- **Surface** (#f7f6f2 / #1c1f22): form fields. **Surface 2** (#e2dfd6 / #24282c): hover on secondary controls.
- **Muted** (#5a5e63 / #9da0a4): meta text. **Rule** (#c9c5ba / #33373c) and **Rule Strong** (#9e998c / #50565c): row dividers and control borders.
- **Bar** (#1e2124 / #0e1011): the top bar stays dark in both themes, with its own text, field and rule tokens.
- **Focus** (#1f5fd1 / #7aa7ff): focus rings only.

### Named Rules
**The Signal Is State Rule.** Red, green and amber appear only to report state. Never use them for decoration, branding, or button fills (the one exception is the danger button's soft red, which marks a destructive act).

**The Line Is Identity Rule.** Project line colours mark which project something belongs to, as a short swatch or a track rule. They never mean state, and are never used as fills for cards.

**The Never Glyph Or Colour Alone Rule.** Every state glyph or hue is paired with a visible text label next to it (status word, "live"/"stale", "N blocked", "Held by", "lease 12m"). A glyph or colour by itself is a defect; a screen-reader-only label does not count for sighted reading.

## Typography

**Display / Body Font:** system-ui stack (`system-ui, -apple-system, "Segoe UI", Roboto, sans-serif`)
**Mono Font:** `ui-monospace, SFMono-Regular, Menlo, Consolas, monospace`

**Character:** Plain, native and legible at a glance. One family at one dominant size, differentiated by weight, case and the mono face rather than by scale.

### Hierarchy
- **Page title** (600, 22px, 1.25, -0.01em): task page headline. **Page title, list** (600, 20px): list page headings.
- **Emphasis** (700, 15px): wordmark and overview counter values.
- **Headline** (600, 18px, 1.3): inbox reader subject.
- **Title** (600, 14px, 1.35): card titles, message subjects.
- **Body** (400, 14px, 1.45): default. Peer text is capped near 80ch, page heads near 75ch.
- **Meta** (400, 12.5px): card meta, owners, event lines, previews.
- **Label** (700, 12 to 12.5px, +0.05em, uppercase): column headers and section/panel titles only.
- **Mono** (12 to 13px): agent ids (`project/host/harness`), lease time left (600), keys, code.

### Named Rules
**The Mono Means Machine Rule.** Monospace is for identifiers an agent produced (ids, leases, tokens). Prose and human labels stay in the sans.

**The Tabular Rule.** Numerals are tabular (`font-variant-numeric: tabular-nums`) on the body so counts and lease times do not jitter on live updates.

## Layout

A single 1440px-max page with 16px padding (12px on phones). The board is a horizontal grid of five columns (open, claimed, blocked, review, done), each at least 220px wide, 8px gutters, scrolling horizontally with scroll-snap; at 520px and below each column is 86vw with mandatory snap. Task page is a content column plus a 260 to 340px actions column (32px gap), collapsing to one column at 820px. Inbox is a 2fr/3fr list-and-reader split with 24px gap, also collapsing at 820px, with the reader first when open. Overview agent rows are a five-column grid that reflows at 1100px and again at 520px.

Spacing rhythm is 4, 8, 12, 16, 24. Card stacks use 8px gaps; sections separate by 24px. Density is a requirement: a change may not fit fewer items per screen or add clicks to an intervention.

## Elevation & Depth

Flat. There are no shadows anywhere. Depth is tonal: ground, then the band (a darker or lighter wash for a column), then the card sitting on it with a 1px border. The only floating element, the board key popover, uses the card surface and border plus `z-index: 5`. State changes show as border or background shifts (card hover darkens the border; a blocked card turns soft red), not lift.

### Named Rules
**The Flat Always Rule.** No `box-shadow`. Separation comes from tone and 1px borders.

**The Selected Is An Outline Rule.** Keyboard selection and focus are a 2px ink (or focus-blue) outline with a 1px offset, never a fill.

## Shapes

Quiet geometry. Cards, panels, columns, lists and the reader use a 6px radius; buttons and the segmented control 4px; form fields and the error bar 3px. State glyphs are 10px circles, except blocked and urgent which become a 1px-radius square, so shape carries state without colour. Track rules and line swatches are straight bars (2px track, 4px swatch, 8px lease occupancy bar). Dividers are 1px rules; nothing is rounded beyond 6px except the circular glyphs. The 1px `hairline` radius is used only on the blocked square glyph. The one literal `#000` in the stylesheet is the opaque stop of the phone project-filter fade mask (`mask-image`); it is an alpha mask, not a colour.

### Named Rules
**The Shape Carries State Rule.** Ring is open, half-filled is claimed, square is blocked, ringed centre is review, solid is done or live, diagonal slash is cancelled, a dashed red ring is planning (block reserved, work not started) and a dashed amber ring with a centre is a plan awaiting the human. Do not reuse these shapes for non-state meaning.

## Components

### Buttons
- **Shape:** 4px radius, 30px tall, 12px horizontal padding, 13px/600 label.
- **Primary:** ink fill with ground-coloured text. Hover shifts to muted; active nudges 1px down (disabled under reduced motion).
- **Secondary:** card fill, ink text, Rule Strong border; hover to Surface 2 with ink border. Used for Force release, New task when open.
- **Danger:** soft red fill, danger text and border; hover fills solid danger. Used for Cancel task.
- **Link:** bare muted text for Log out and low-stakes actions.
- **Key button:** the board Key control is a `summary` styled as a secondary button; open, it takes the band fill and an ink border, and its popover (card fill, Rule Strong border, 6px below the button) lists every glyph with a word.
- **Always labelled:** every button carries a verb label ("Post as you", "Create subtask", "Cancel task"). Never an icon-only button. Destructive forms confirm via JS.

### Cards (task card)
- **Corner / border:** 6px radius, 1px Card Border, Card fill, 10px 12px padding, 5px internal gap.
- **Content order:** title link (600, persistent 1px Rule Strong underline offset 3px that turns to ink on card or link hover), optional "Subtask of ..." line, state + project row, owner row ("Held by" or "Done by" + mono agent id + live/stale), lease meter, subtask progress.
- **State:** a blocked card is soft red with a red border. Hover darkens the border and the title underline.
- **Selection:** j/k moves a 2px outline between `[data-nav]` cards.
- **Compact density:** one 30px row (signals wrap to a second line rather than being clipped in a narrow column) (4px 8px padding, 12px meta, 13px title): state glyph + status word, the title on one line with an ellipsis and the full title as its tooltip, the project only on an all-projects board, the holder in mono (or "open"), "lease 12m left", then text chips (warn or danger soft fill, 1px border, 3px radius, 11.5px/600) for urgent, plan awaiting you, blocked on you, N blocked, N review and a low or expired lease, and the mono "done/total" count. The title shrinks first. A Done row's Ack sits beside it as a 24px secondary button.

### Needs you
An amber band (Warn Soft fill, Caution Amber 1px border, 6px radius) at the top of the board and the overview, rendered only when something waits on the human. Its head mirrors a column head (glyph, uppercase label, count in warn at 18px bold) over a 2px amber rule. Inside, a responsive grid of attention cards (card fill, 2px amber border, red border and soft red fill for urgent mail), each led by the verb in uppercase warn text beside its glyph ("Approve plan", "Review", "Reply"), then the title link, project, agent and "waiting 12m", a clipped preview, and the action (Approve plan as primary, Request changes as an inline disclosure, or a labelled link). Live refreshes skip the band while a change request is open or focused. The top bar's "N need you" equals the card count and links to it.

A **work request** card leads with "Work request · project ← agent · age", shows the title and first line of the body as framed peer text, and carries Approve request (primary), Decline and Reply as inline disclosures with labelled submit buttons. On the board, requests sit in a **Requested** lane (dashed amber ring beside the word) at the top of the open column; an approved task waiting for a session shows a "queued" badge (dashed ink ring beside the word).

### Plan panel
On the task page, above the lease: the latest plan as peer text in a card-filled box. While it awaits the human the panel takes the Needs-you treatment (warn soft fill, 2px amber border, warn title with the plan-awaiting glyph) and carries Approve plan plus a Request changes form with a required note.

### Columns
- **Band:** tinted wash, 6px radius, min-height 160px. Header is 40px with a 2px ink bottom rule, the column's aspect glyph, an uppercase label and a count at the right. The 2px head rules join into one continuous line across the board: each non-last head extends 8px into the gutter with a 2px by 10px vertical joint, so gutters read as junctions between blocks.
- **Done head:** the count reads "N unseen" (not a bare number). Under the rule sit a labelled **Ack all** button and a **History** link; when work has auto-acked, a muted line "N auto-acked, unseen" links to it and is hidden at zero. Each done card has a labelled **Ack** button beneath it, outside the card.
- **Waiting work shouts:** when the blocked or review column has cards, its glyph grows to 14px and its count to 18px bold in danger or warn. Empty columns stay quiet with a sentence of explanation.

### Lease meter
A project-coloured 2px track with an 8px occupancy bar whose length is time left on the lease (red; amber when under a fifth remains; zero when expired), beside text "lease" plus mono time left. JS decrements it between server renders.

### Subtask stations
A row of 10px station glyphs joined by 2px ink segments, one per child, followed by "N of M subtasks done" (child rows show the status word plus owner only when one exists, with no "(open)" filler) and, when relevant, "N blocked" with the blocked glyph. "Show N subtasks" expands a vertical station list joined by a 2px rule.

### Signal aspect + label
The universal state mark: a 10px glyph followed by its word (`.state`, `.presence`, `.importance`, `.token-state`). Text colour uses the text-safe twin for blocked, review, done and stale.

### Inputs / Fields
- **Style:** 30px tall, 1px Rule Strong border, 3px radius, Surface fill, 5px 9px padding. Labels sit above in muted 12px/600.
- **Hover / Focus:** border goes to ink on hover, to Focus blue with a 2px outline on focus.
- **Error:** soft red bar with danger text, 8px 10px padding.

### Navigation
Dark top bar, 44px: wordmark with a track-and-red-square mark, text tabs (active has a 2px bottom rule and 600 weight), a filter field, a live pulse of counts with glyphs, and Log out. On phones the tabs wrap to their own row. Board project filters are a row of "line swatch + project name" links; the current one gets a card fill and a thicker swatch. The Key button opens a popover explaining every glyph in words.

### Counters and lists
Overview counters are one ruled line of label/value pairs inside a card; an alert counter turns danger. Agent rows, project rows, message lists and event logs are card-filled lists with 1px row dividers; unread messages are bold with a dot, urgent messages carry the soft red wash.

## Do's and Don'ts

### Do:
- **Do** pair every signal glyph or state hue with a text label right beside it.
- **Do** keep red, green and amber for state, and use the text-safe twins (`danger`, `ok`, `warn`) for type.
- **Do** colour a project's track rule or swatch with its line colour, consistently across surfaces.
- **Do** surface tasks as bordered cards on column bands, 6px radius, no shadows.
- **Do** label every button with a verb; use ink for primary, bordered for secondary, soft red for destructive.
- **Do** use tabular numerals and mono for agent ids and lease times.
- **Do** make the blocked and review columns louder only when they hold work.
- **Do** define colours as tokens on `:root` with a dark `prefers-color-scheme` override; add new roles to both.

### Don't:
- **Don't** convey state by glyph or colour alone; a lone dot is a bug.
- **Don't** use `box-shadow`, gradient glows or a decorative accent hue.
- **Don't** use project line colours to mean status, or signal colours to mean project.
- **Don't** add icon-only buttons or external fonts, scripts, styles or images (strict CSP, one binary).
- **Don't** nest cards inside cards; panels and lists are one level on the ground or band.
- **Don't** add chrome, extra clicks or padding that reduces how much state fits on screen.
- **Don't** dress it as a terminal or a generic SaaS dashboard.
