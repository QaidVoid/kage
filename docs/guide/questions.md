# questions

When a decision is yours to make, the model can ask instead of
guessing. It calls the `ask_user_question` tool with one to four
questions, and its run waits until you answer.

Each question has a short header, the question itself, and two to four
choices, each with a label and a description of what picking it means.
A question may allow several choices. You can always answer in your
own words instead, or skip.

```json
{"questions": [{
  "header": "Store",
  "question": "Where should sessions live?",
  "options": [
    {"label": "Disk (Recommended)", "description": "Survives restarts"},
    {"label": "Memory", "description": "Faster, lost on exit"}
  ]
}]}
```

Only the main session gets the tool, and only when a client can
answer: the TUI, the desktop and browser clients, and editors over
ACP. Agents report to the session that started them instead, and
print mode has nobody to ask. The tool never asks for approval first,
since answering is your say. A `deny` rule for `ask_user_question` in
`[permissions]` still refuses it. It works in plan mode too.

## in the TUI

The questions take the place of the input, one at a time:

```text
-- Store (1 of 2) -----------------------------------------------------------------------------
   Where should sessions live?

 > 1. Disk (Recommended)  Survives restarts
   2. Memory  Faster, lost on exit
   3. Answer in my own words
-----------------------------------------------------------------------------------------------
  1-9 or enter . esc to skip
```

| Key | Effect |
| --- | --- |
| `1` to `9` | Pick that row. On a question that allows several choices, a choice toggles. |
| `up` / `down`, `k` / `j` | Move the selection |
| `enter` | Act on the selected row |
| `space` | Toggle the selected choice, when several may be picked |
| `esc` | Skip, which declines every question of the call |

A question that allows several choices shows `[x]` on the picked
ones and ends with a `Done` row. `Answer in my own words` opens a
one-line field: `enter` sends it and `esc` goes back to the choices.
While the questions wait, the working row reads `Waiting for your
answer`.

## in the desktop and browser clients

The question shows as a card above the composer. A click on a choice
answers it. On a question that allows several choices, clicks toggle
them and `Done` sends them. The field under the choices takes an
answer in your own words, and `Skip` declines. The session's row in
the sidebar shows `Answer` while a question waits.

## in editors over ACP

Each question reaches the editor as a `session/request_permission`
whose title is the header and the question, with one option per
choice and a `Skip` option, so any editor can answer it with one
choice. See [zed](/editors/zed#questions) for the wire details.

## what the model reads

The tool result pairs each question with your answer:

```text
The user answered:
- Where should sessions live?: Memory
- Which export formats?: JSON, YAML
```

A skipped question reads `The user declined to answer.`, and the model
goes on with its best judgement or asks in its reply instead.
