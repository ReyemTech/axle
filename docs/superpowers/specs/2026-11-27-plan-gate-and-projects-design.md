# Plan gate and multi-project tasks — design

Status: approved by the owner on 2026-11-27. Extends `2026-11-27-agent-coordination-design.md`.

## Why

The owner creates cross-project work from the web UI (e.g. platform-infra + web-app). Today the UI form only picks one project, so a UI-created task can never be split across projects, because a parent's participating projects are fixed at creation. The owner also wants the lead agent to write a spec or plan and get it approved in the UI before doing the work.

## Part 1 — participating projects

- **New-task form:** `Lead project` (single select; the task's `project`, where the coordinator claims it) plus `Also involves` (checkbox list of the other known projects → `participating_projects`). Only on top-level tasks.
- **Lead can extend:** projects are append-only after creation. A parent task's current owner, its creator, or the human (UI) may add projects:
  - MCP: `task_update` gains `add_projects: [project keys]`. It is rejected on child tasks (`ChildExtraProjects`), and an unknown project key is rejected. Adding a project that is already present is a no-op.
  - UI: a "+ project" control on the task page (parent tasks only), CSRF-protected.
  - Each addition posts a system note in the task thread ("added project X") and emits a task event, so the new project's agents see it.
- Removal is not supported, so children that already exist always stay valid.

## Part 2 — plan gate

- **Opt-in per task:** new field `requires_plan` (bool, default false). The UI form has a "Plan first — I approve before work starts" checkbox, **ticked by default**. MCP `task_create` accepts `requires_plan` too, but agents normally leave it off.
- **Plan state:** `plan_state` on the task: `none | drafting | submitted | changes_requested | approved`. Tasks without `requires_plan` stay `none`.
- **Status:** new status `planning`.
  - Claiming a `requires_plan` task moves it `open → planning` (plan_state `drafting`) instead of `claimed`.
  - The claim response tells the agent: write a spec/plan in the thread with `task_propose_plan`, then wait for approval. For a parent spanning projects, the plan should list the subtasks per project.
- **New MCP tool `task_propose_plan { id, plan }`:**
  - Only the owner may call it, and only while the task is `planning`.
  - It stores the plan as a thread message of kind `plan` addressed to the human, with importance high, and sets plan_state `submitted`.
  - Calling it again supersedes the earlier plan; the latest plan is the one under review.
- **Human decision** (UI task page, CSRF):
  - **Approve:** plan_state `approved`, status `planning → claimed`. Posts a system message to the owner session ("plan approved"), and the wake adapters deliver it.
  - **Request changes (note required):** plan_state `changes_requested`, status stays `planning`. Posts the note to the owner.
- **Server enforcement while `planning`:** these are rejected with a clear `PlanNotApproved` error (the MCP message says what to do next):
  - creating children under the task
  - `task_complete`
  - `task_update` to `review`
  - `task_update` to `blocked`
- **Still allowed while `planning`:** `task_post`, `task_release`, `task_renew`, and `force_release`/`cancel` from the UI. Release returns the task to `open` and keeps `requires_plan`. Its plan_state resets to `none` and becomes `drafting` on the next claim; old plans stay in the thread.
- **Lease:**
  - While plan_state is `submitted`, the lease does not expire, just as for blocked and review, because the task is waiting on the human.
  - In `drafting` and `changes_requested`, the normal lease and renewal rules apply.
- **UI:**
  - **Board:** `planning` tasks sit in the CLAIMED column with their own aspect and label: "planning", or "plan awaiting you" when submitted.
  - **Pulse/top bar:** adds an "N need you" count, covering plans awaiting approval plus human-addressed urgent mail. It links to a filtered view.
  - **Task page:** a plan panel shows the latest plan rendered as text (peer-data framing kept), with Approve and Request changes. Earlier plans stay in the thread history.
- **Key/legend:** gains the planning aspect.
- **Needs you section:** a prominent section at the top of the board and the overview, shown only when it has items. It holds high-visibility attention cards, each with a label and glyph, for three things:
  - plans awaiting approval, with Approve and Request changes one click away
  - human-addressed urgent or high messages
  - tasks in `review`
  
  Each card states the action ("Approve plan", "Reply", "Review"), the task, project and agent, and how long it has waited. It updates live. The top-bar "N need you" count equals the number of cards and links to the section.

## Constraints

- Both stores (SQLite and Postgres) and new migrations `0002_*` for both; do not edit `0001_init.sql`.
- No identity parameters on tools; same peer-data framing and size caps; the same CSRF and CSP rules as the rest of the UI; follow DESIGN.md (no glyph or colour alone).
- Tests: store tests on both backends for the new transitions and guards, MCP tool tests, UI tests (form fields, approve/request-changes, add-project), and a test that the submitted plan lease does not expire.
