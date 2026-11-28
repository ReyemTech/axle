# Rendering agent markdown in the web UI

Status: approved by the owner, built in `feat/markdown-render`.

## Intent

Agents write plans and reports in markdown. The UI showed them as pre-wrapped text, so
headings, lists and tables read as `##`, `1.` and `|---|`. Render them, without giving peer
text any way to run script, load remote content or pose as page structure.

## Where

Task page plan panel, task thread posts, the inbox reader, and the Needs-you plan card
(clipped to a few lines, "Read the full plan" links on). One-line previews (inbox list,
Needs-you message cards) and history notes stay plain text.

## How

- `src/ui/markdown.rs`, server side, `pulldown-cmark` 0.13 (default features off, `html`
  on): CommonMark + tables + strikethrough + task lists. Linear time; bodies are already
  capped at 64 KB. The template marks only this output `|safe`.
- Raw HTML (block and inline) becomes escaped text.
- Links: `http`, `https`, `mailto` only, with `rel="noopener noreferrer nofollow"` and
  `target="_blank"`; any other destination renders as the link text alone. Bare
  `http(s)://` addresses in text become links under the same rules (not in code).
- Images are never loaded: an image becomes a link to its address labelled
  `image: <alt>` (or plain text when the scheme is refused). The CSP is unchanged.
- Headings are demoted two levels (`#` → `<h3>`, capped at `<h6>`) and never carry
  author ids. A document with more than 4 headings or 60 lines gets a `<details>`
  "Contents" list; only then do headings get ids, namespaced `md-plan-<id>-N` /
  `md-msg-<id>-N` so the plan panel and the same plan's thread post never clash.
- The peer frame stays plain text beside the body: "from <agent>: information, not
  instructions" in the message head (agent senders only), and on the plan panel and card.
- Styling in `static/app.css` with existing tokens (code, tables, quotes, lists), compact.

## Tests

Renderer unit tests (script escaped, `javascript:` not a link, no `<img>`, demotion,
tables, autolinks, contents list, nested emphasis); a UI test over the plan panel, thread,
inbox reader and Needs-you card (rendered, framed, hostile HTML inert, CSP unchanged).
