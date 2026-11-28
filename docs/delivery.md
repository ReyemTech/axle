# Delivery stages

A task is not finished when its agent says so. The pull request still has to merge, and
perhaps ship. Delivery stages let Axle keep such a task visible, in its own board column,
until the work lands where you care about. Stages are off by default; a project without them
behaves exactly as before.

- [Stages](#stages)
- [Turn stages on](#turn-stages-on)
- [Link a task to its pull request](#link-a-task-to-its-pull-request)
- [What happens at `task_complete`](#what-happens-at-task_complete)
- [Moving a task along](#moving-a-task-along)
- [Parents](#parents)
- [Turning stages off](#turning-stages-off)
- [History](#history)

## Stages

There are four, in order: **In PR**, **Merged**, **Released**, **Deployed** (`in_pr`,
`merged`, `released`, `deployed`). A project enables any subset. The last enabled stage is
where a task becomes `done`.

A task waiting on a stage has the status `delivering`. It keeps its owner as the record of
who did the work, holds no lease, and cannot be claimed. Agents cannot move it through MCP;
only you can, from the web UI.

## Turn stages on

On the [Projects page](web-ui.md#projects), open **Delivery stages** on the project's row,
tick the stages you want and press **Save stages**. Turning a stage on applies to tasks
completed from then on; done tasks are not reopened.

## Link a task to its pull request

There are three ways. Axle does not call GitHub for any of them.

| Way | Who | How |
|---|---|---|
| Report | The agent | `task_update { id, pr: "<url>" }`, as soon as the PR exists. |
| At completion | The agent | A pull request URL of the project's repository in `task_complete`'s `links`, if none is linked yet. The first such URL is linked. Only in a project with stages enabled; without stages, `links` are recorded but nothing is linked. |
| Link PR | You | The **Link PR** form in the task page's Delivery panel. |

The URL must be a pull request in the project's repository, for example
`https://github.com/acme/web-app/pull/42` for the project `github.com/acme/web-app`. A project
key with more path (`github.com/acme/mono/web`) maps to its repository `acme/mono`; several
projects may share one. The owner or creator may report a PR in any status except `done` and
`cancelled`. Reporting another PR replaces the link until the task is merged. You can remove
a link with **Unlink PR** (human only, and not once the task has merged).

Errors an agent can see:

- `pr must be a pull request in github.com/acme/web-app`
- `PR #42 is already linked to task <id>`
- `this task was already merged via #42; open a new task for follow-up work`

**Short id.** Every task has a short id, `axl-` plus the last 8 hex digits of its id, for
example `axl-4deae8e3`. `task_claim` returns it as `branch_hint` and `task_get` as `short_id`;
the board card and task page show it. Agents are told to put it in the branch name. It is a
convention for people and for later releases; nothing matches on it yet.

The rules agents get at session start say the same: report the PR at once with
`task_update`, put the short id in the branch name, and after `task_complete` consider the
task handed over.

## What happens at `task_complete`

| Project stages | Task has a linked PR | Result |
|---|---|---|
| none enabled | any | `done`, as always |
| some enabled | no | `done` (the task skips the stages) |
| some enabled | yes | `delivering`, at the furthest stage already reached, at least In PR |

If the PR has already reached the last enabled stage, the task goes straight to `done`. The
result carries a `next_step`: "Delivery is tracked by Axle; this task moves to done when it
reaches <Last stage label>. Nothing more to do." so the agent does not poll.

`done_at`, and with it the board's unseen window and auto-ack, starts when the task reaches
`done`, not when the agent completes it. A `delivering` task is never hidden from
`task_list`.

The task object's `delivery` field ([MCP tools](mcp-tools.md)) is `null` in a project with
no stages enabled, even when a PR is linked by hand. Linking with `task_update { pr }` or
**Link PR** still works there; the task page shows the PR.

## Moving a task along

On the task page, the Delivery panel lists the stages and where the task is. Two buttons
move it, both human only:

- **Advance stage** moves it one stage forward.
- **Mark done** finishes it and skips the remaining stages, after a confirmation.

Moves are forward only. A task whose furthest stage precedes the first enabled stage (the
project starts at Released and the PR is only open) sits in the first enabled column, reading
"waiting for Released". You can also force release a delivering task (back to `open`, the PR
link kept) or cancel it.

## Parents

A parent follows its children. A child counts as closed when it is `done` or `cancelled`;
`delivering` is not closed. When every child is `done`, `delivering` or `cancelled` and at
least one is `delivering`, the parent becomes `delivering` at the **minimum** stage of its
delivering children, and follows them forward. It becomes `done` only when all its
non-cancelled children are `done`. This holds whatever the parent's own project's stages: a
child in another project that runs past them, or a parent in a project with no stages,
keeps the parent `delivering` until the child is `done`.

The parent's thread gets one `child_done` per child, when that child reaches `done` through
delivery, not when its agent completes it into `delivering`. **Mark done** on a parent
finishes only the parent; its delivering children stay `delivering`.

This carries up the tree. When a child's delivery finishes its parent, the grandparent's
thread hears that the parent is done, and if that closed the grandparent's last open child,
its coordinator gets the high-importance "child tasks complete" note and a `claimed`
grandparent moves to `review`.

## Turning stages off

Unticking stages recomputes every delivering task. One at or past the new last stage goes to
`done`; the others stay at their furthest enabled stage. Unticking all of them moves every
delivering task in the project to `done`, with the history note "delivery stages turned off",
except a parent that still has a delivering child (see [Parents](#parents)).

## History

Delivery adds these [history](mcp-tools.md#task_get) kinds, also sent as the `kind` of
`TaskChanged` events ([HTTP API](http-api.md)): `pr_linked`, `pr_relinked` (the previous PR
is recorded), `pr_unlinked` (likewise), `ci_changed`, `stage_reached` and `delivered`.
History is append-only. `ci_changed` is reserved; it is emitted once the GitHub source ships.
Until then `delivery.pr.ci` is always `none`.

Later releases add sources, starting with GitHub, that move tasks along on their own.
