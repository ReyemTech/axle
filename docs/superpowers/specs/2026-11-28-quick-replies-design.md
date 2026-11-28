# Quick replies

Status: approved by the owner (short design), built on `feat/quick-replies`.

## Problem

Agents often ask the human a question with two or three obvious answers ("deploy now or
wait?"). Answering means opening the message and typing the answer out.

## Design

- **MCP.** `send_message` takes optional `options`: 2 to 4 strings, each 1 to 80 characters
  after trimming, on one line, no two alike ignoring case. Anything else is refused with an
  `options: …` error. An empty list is the same as none.
- **Human only.** Options on any other recipient are refused (`options are only for messages
  to the human`): only the human has buttons to press, and accepting them silently elsewhere
  would let an agent believe it had offered a choice that nobody sees.
- **Storage.** Migration `0005_message_options` adds a nullable `messages.options` column in
  both stores, holding a JSON array. Messages stay append-only; options are never edited.
- **Output.** Message JSON gains `options` (an array, empty for most mail), each entry defused
  of frame markers like the body. The human's answer is an ordinary message, so the agent
  receiving it needs nothing new.
- **UI.** The inbox reader and the message's Needs you card show "Quick reply:" and one
  secondary button per option, full text on the button, escaped by askama. The free-text box
  stays underneath (on the card, an optional note). A button posts to the same
  `/ui/messages` handler as a typed reply (CSRF, `reply_to` threading, the human's copy
  archived in the same write), with `option` set; the body is the option, or
  `<option>\n\n<typed text>` when the box has text. The card's form returns to its page.

## Not in scope

Options on task threads or agent mail, recording which option was picked as structured data,
and editing or retracting options.

## Merge note

Numbered `0005`, the next free number after `0004_done_ack` on main.
