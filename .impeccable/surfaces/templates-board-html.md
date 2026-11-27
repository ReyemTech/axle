---
version: 1
slug: "templates-board-html"
primary_target: "templates/board.html"
related_targets: ["templates/base.html","templates/overview.html","templates/task.html","templates/inbox.html","templates/login.html","static/app.css"]
---

# axle web UI — surface brief

Scope: the whole axle web UI (login, overview, projects, board, task thread, inbox, tokens, error). Mode: Operate. Primary surface: the kanban board. Visitor: the solo owner, UI open all day on a second screen as a live control room. Must keep or improve density; no added clicks; strict CSP (no external fonts/scripts/styles); tiny page weight; light + dark via prefers-color-scheme.

Avoid (user): generic SaaS dashboard (gradient glow, cards everywhere), hacker-terminal costume, heavy page weight, losing density.

## Direction contract

THESIS: One train per block. The board is a railway track diagram where a claim lights its block until the lease clears; it refuses the category default of neutral cards in soft-bordered columns with status pills.

OWN-WORLD: Light ground #EDEBE4 (dark: #16181A), track ink #1E2124, signal aspects as the only saturated colour with jobs: red #D7263D occupied/blocked/urgent, green #3BB273 clear/done/present, amber #F0A202 caution/review/stale; each project is a "line" with its own line colour used on its track rule. Hairline 1px track rules, no shadows, no boxes-in-boxes; signal-aspect dots (filled, half, ring) as state glyphs; tabular numerals; system UI stack plus ui-monospace for ids.

STORY: The owner sees at a glance which blocks are occupied (claimed), by whom and for how long, what is blocked or awaiting review, and who is live; they post, force-release or cancel in place.

FIRST VIEWPORT: Top bar: axle wordmark, project lines as coloured track segments (nav), live count and blocked count at right. Below: five columns OPEN / CLAIMED / BLOCKED / REVIEW / DONE separated by track rules. Each claimed card shows an occupancy segment (bar) draining with lease remaining (length = time), owner agent@host. Parent tasks render children as stations on a line ━●━◐━○━ with n/m. Primary action: open task / claim actions inside card and task page.

FORM: Block Signalling — railway track diagrams and absolute block signalling; position 1 on the ordered list (IMPECCABLE'S PICK, chosen over the assigned roll). seed:733433137f427578. Raises kept: 1px ruled dividers (plate section); visible grid armature (Crouwel); lease as exact-length bar (labanotation); one --lease custom property drives bar/fade (variable specimen); snap between whole states (vertical feed).

FINISH: unreviewed and undocumented is unfinished; this build ends with the finish review, the verdict, DESIGN.md, and every shipping raster carrying its provenance
