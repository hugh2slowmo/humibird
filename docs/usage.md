# Using humibird

The guide to the app. For building it, read [development.md](development.md).

## First run

1. Start the app (`cargo run`, or open `Humibird.app`).
2. Press `⌘,` for **Settings**. A fresh profile is already on DeepSeek; paste a
   key, or fill one in for whichever provider you want, and press Enter.
3. Pick a workspace with the control in the header — or keep the profile's
   default — and type.

There is no account, no sign-in, and no wizard. An empty thread with no key says
so and offers Settings.

## The window at a glance

| Area | What it does |
|---|---|
| Sidebar | **New thread** (`⌘N`), then this profile's threads: this workspace first, then the rest. The bottom row opens Settings. |
| Header | The thread's title, its workspace (click to choose another, which starts a new thread), and the light/dark toggle. |
| Timeline | Your message, the work the agent did, then its answer. |
| Composer | The input, a **Think** switch with its effort, the context ring, a **Model** chip, and a send button that becomes stop while a turn runs. |

Threads are named by the model from the first exchange; if it answers nothing
usable, the first line of your message stands in. Hovering a row shows a delete
control: the first press asks, the second removes the thread and everything its
subagents stored. There is no undo.

The agent can recall earlier conversations with `session_search` when you ask
about past discussions or decisions. It searches user and assistant message
bodies in the current workspace, then reads a bounded passage around a match.
It can explicitly include other workspaces in the same profile. The current
conversation, subagent sessions, attachments, and tool output are excluded.
Recalled excerpts are historical reference data and may be outdated.

## Providers and keys

| Provider | `[provider] id` | Key variable | Notes |
|---|---|---|---|
| DeepSeek | `deepseek` | `DEEPSEEK_API_KEY` | the default; ships a small model catalogue |
| OpenAI | `openai` | `OPENAI_API_KEY` | models are asked for on first use |
| OpenRouter | `openrouter` | `OPENROUTER_API_KEY` | |
| Anthropic | `anthropic` | `ANTHROPIC_API_KEY` | its own API, not the OpenAI wire |
| Ollama | `ollama` | — | `http://localhost:11434/v1`, no key |
| Custom | `custom` | — | any OpenAI-compatible endpoint; you name the URL |

Keys entered in Settings go to the profile's `.env` with `0600` permissions —
plain text, because a profile has to be copyable and there is no keychain in the
loop. The file wins over the shell: a variable exported for another tool does not
silently replace the key you stored for this profile. The environment is the
fallback, so a launch that has no key yet can borrow `OPENAI_API_KEY` from the
shell.

**Choosing a model.** The chip lists every provider that can answer, grouped: a
provider whose key is stored shows the models the app catalogues for it, plus
whatever it names through `GET /models` — every reachable provider is asked when
the list opens, once per provider and once per key, with a ten-second deadline.
The list takes the keyboard: `↑`/`↓` move, Enter picks, Escape or a click
elsewhere closes it. A pick applies from the next turn, is recorded on the thread,
and becomes the default for new threads. The list ends with **Configure
providers…**, which opens Settings → Providers: the chip is also drawn when the
provider you are on cannot answer, so the way to add a key is never more than one
click away.

**Switching providers.** A provider with a key appears with its own models, so no
separate switch is needed: picking one of its models moves the thread to it. Each
provider keeps its own endpoint, key variable and window, so switching away and
back does not lose what `custom` was told. A key saved or reset in Settings is
picked up by the next open of the list. **A provider with nothing to offer is not
listed**: Ollama appears once its address is named and its server answers, and
`custom` once its section names an endpoint and a model. Settings holds the keys;
which provider is in use is decided here, by the model you pick.

Ollama and `custom` have no catalogue of their own. Neither is probed until its
section names an address — a profile that never mentions Ollama never sends it a
request — and Ollama's default address is the placeholder in its field. A `custom`
endpoint names its model as well, because nothing about it can be known in advance;
both sections are editable whether or not the provider is the one in use. Neither
reports a context window unless its listing does, so a `custom` endpoint that wants
the ring and automatic compaction names a window in Settings, or
`[providers.custom] context_window` in `config.toml`.

## Thinking

**Think** in the composer asks the model to reason before it answers; while it is
on, a chip names the level (Low, Medium, High) and clicking it opens *Change
thinking effort*. The setting is saved in `config.toml` and applies from the next
turn — it names nothing in the prompt, so changing it costs no cached prefix.

- **DeepSeek** gets an explicit `thinking` field and an effort; switching it off
  is a `disabled` the model honours.
- **OpenAI** and **OpenRouter** get `reasoning_effort` only, so switching off
  means sending nothing.
- **Anthropic, Ollama and custom** have no switch here, and the control is not
  shown: Anthropic's thinking only works if its blocks are replayed in later
  requests, and this app never stores them.

Reasoning is shown when the provider streams it, and is never stored or sent back.

## Workspaces

An agent that can run shell commands needs a boundary, and the boundary is the
workspace a thread names. Every path a file tool takes is relative to it; absolute
paths, `..`, and symlinks that leave it are refused.

- A fresh profile's workspace is `<profile>/workspace`, created on first use.
- Choosing a folder in the header starts a **new** thread there; the thread you
  were reading keeps its workspace.
- `/`, your home directory on its own, and the profile's own tree are refused, and
  the composer says why.

`find_path` and `grep` skip what `.gitignore` excludes; `list_directory` still
shows it, so a hidden directory can be reached by asking for it by name.

## Talking to the agent

- **Send** with Enter. With no model selected, Enter keeps the draft and opens the
  model list instead of starting a turn that nothing can answer.
- **Stop** replaces Send while a turn runs. Stopping keeps the text that already
  streamed, marks the turn stopped, and never writes a half-written reply.
- **Queue** by sending while a turn runs; the message is shown above the input and
  sent next, and it survives a stop.
- **Steer** the front queued message to end the running turn at its next message
  boundary. It is still the next turn, so the transcript stays append-only.
- **Answer** an `ask_user` card above the composer: options, an optional text
  field, and Skip. The turn waits for you.

### Files and images

`+` attaches files to the message being written. A text file reaches the model
inside a `<file name="path">` block, named by its workspace path when it is there;
an image is sent as an image. A screenshot on the clipboard can be pasted with
`⌘V`. Limits: four attachments per message, 256 KiB per text file, 4 MiB per
image — anything larger is refused above the composer and can be pointed at
instead, which leaves the reading to `read_file`.

Attachments belong to the turn and are kept in the transcript, so a reopened
thread sends them again. `@` names a file without opening the picker: type `@`
and a few letters to see what matches in the workspace, Enter takes the top
match, and the mention becomes an attachment.

## Approvals

Read-only calls run directly: `read_file`, `list_directory`, `find_path`, `grep`,
`memory`, `read_skill`, and `web_search`. Everything that changes the workspace or
reaches the network asks first:

| Tool | Approval key |
|---|---|
| `write_file`, `edit_file`, `delete_path`, `move_path` | the tool |
| `terminal` | the tool |
| `fetch` | one key per host, so approving a page does not approve the internet |
| a context server's tools | one key per tool (`mcp:<server>:<tool>`), so approving `search` does not approve `delete` |

Each prompt offers **Allow once**, **Always this session**, and **Deny**. The
session allow-list belongs to the window, so a second write — or a second page on
the same host — does not ask again, and nothing is remembered after a restart. A
denial is a tool result the model can read, never a silent drop. The prompt pins
above the composer and blocks the turn; waiting subagents are asked one at a time.

The workspace check is the real security boundary; approval is the second one.
Neither is a sandbox: an approved `terminal` command runs as you.

## Context servers (MCP)

A context server adds tools the app does not ship — a filesystem outside the
workspace, an issue tracker, a browser. They are configured per profile in
`mcp.json`, next to `config.toml`:

```json
{
  "servers": {
    "notes": {
      "command": "npx",
      "args": ["-y", "@modelcontextprotocol/server-filesystem", "/Users/me/notes"],
      "env": { "SOME_TOKEN": "…" },
      "timeout": 60
    }
  }
}
```

`enabled` defaults to `true`, `args` and `env` are optional, and a server can be
switched off without deleting it. A server is either a **command**, run as a child
process, or a **`url`**, reached with streamable HTTP:

```json
{
  "servers": {
    "remote": {
      "url": "https://mcp.example.com/mcp",
      "headers": { "Authorization": "Bearer …" }
    }
  }
}
```

`headers` are sent with every request, which is where a hosted server's token
goes. OAuth is not built: a server that answers `401` shows up as a failure line
and needs a static token in `headers`.

The window starts the servers in the background when it opens, and asks each for
its tools; a slow server still reaches the thread that is open, and every thread
opened afterwards starts with them. A server that cannot start is a line above the
composer, and the thread carries on with the tools it has. Tools appear as
`mcp__<server>__<tool>`, and **every call asks for approval** — per tool, never per
server.

Editing `mcp.json` takes effect on the next launch. **Settings → Context servers**
shows what the file names, whether each server came up, and how many tools it
offered.

## Threads and long conversations

Every thread is its own conversation with its own model and workspace, stored as
it goes; closing the app and reopening it restores the thread you were in.

The composer's ring shows how much of the model's window the thread has used —
just a ring, and pointing at it opens a card with `Context` and `20% · 198k / 1M`.
It turns to the warning colour at three quarters, the point where the thread
starts summarizing. A model with no known window shows no ring.

At three quarters of the window the thread summarizes its older turns before the
next request. There is no Summarize button. A summary covers every turn except the
two most recent — and only once there are at least four — and the timeline shows a
banner saying how many turns it stands in for; the messages themselves are still
there to read. If summarizing fails, the turn is sent uncompacted and the failure
is logged. If the provider refuses a request as too long, the next message
summarizes first and the thread carries on.

## Memory

Two Markdown files per profile:

| File | Holds | Budget |
|---|---|---|
| `memories/USER.md` | what the agent knows about you | 1,375 characters |
| `memories/MEMORY.md` | the agent's own notes | 2,200 characters |

Entries are separated by a line containing only `§`, and the budget keeps the
snapshot from growing without bound. The agent edits them through the `memory`
tool (`add`, `patch`, `remove`); you can edit them by hand, and deleting a line is
always allowed. `SOUL.md` in the profile directory is optional standing
instructions.

Memory is **frozen when a thread starts**: mid-session writes go to disk but not
into the request, because rewriting the prefix of a cached conversation is what
makes agent apps expensive. Start a new thread to have it take effect.

## Skills

A skill is a written procedure the model loads when a task calls for it: a
directory with a `SKILL.md` (frontmatter `name` and `description`) plus whatever
files sit next to it.

Only the index is in the system prompt — one line per skill. The body is one
`read_skill` call away, so a profile with twenty skills costs twenty lines, not
twenty pages. A skill's `scripts/` run through `terminal`, so each command needs
its approval.

Install one from **Settings → Skills**, which lists what is in `~/.agents/skills`
and copies a chosen directory into the profile. Copying, not linking, keeps a
profile a directory you can back up. Existing names are never overwritten. The
index is read when a thread starts, so a new skill applies to the next thread;
editing a skill's body is live.

## Subagents

When the agent delegates — through the `delegate` tool — a second conversation runs
with the same profile, workspace, model and skills, but no memory tools, no
`ask_user`, and no further delegation. Its approvals are this window's.

The step that delegated opens into a card: the task, the calls the subagent made,
and its report, with a stop control of its own. What the parent's model sees is the
report alone.

## Files, settings, and logs

Everything a profile owns lives in one directory:

```text
~/.humibird/profiles/default/
├── config.toml       # settings
├── .env              # secrets only
├── SOUL.md           # optional standing instructions
├── memories/         # MEMORY.md, USER.md
├── skills/           # installed SKILL.md trees
├── sessions/         # sessions.sqlite3
├── workspace/        # the default workspace
└── logs/humibird.log
```

`config.toml` holds what the app acts on. Missing keys fall back to the defaults,
unknown keys are ignored so a file from a newer version still loads, and a value
it cannot parse is an error, because a typo should be visible:

```toml
theme = "light"          # light | dark

[provider]
id = "deepseek"          # deepseek | openai | openrouter | anthropic | ollama | custom
model = "deepseek-flash"

# Overrides belong to a provider, whether or not it is the one in use.
[providers.custom]
base_url = "http://localhost:8000/v1"
model = "local-model"
# key_env = "MY_GATEWAY_KEY"   # read the key from this variable instead
# context_window = 131072      # tokens, when the provider does not report one

[thinking]
enabled = false          # the composer's Think switch
effort = "medium"        # low | medium | high
```

A file written before `[providers.<id>]` existed keeps working: `base_url`,
`key_env` and `context_window` under `[provider]` are read as the provider in
use's own, and moving to another provider takes them along to that provider's
record.

Secrets never go in `config.toml`. Both files are read at startup and when the
window saves them, so a hand edit to `config.toml` needs a restart.

To move a profile, copy the directory. To start over, quit the app and delete it —
nothing of yours lives outside it except what the agent wrote in your workspaces.

## Keyboard shortcuts

| Shortcut | Does |
|---|---|
| `⌘N` | New thread |
| `⌘,` | Settings |
| `Esc` | Close the model list, or Settings |
| `↑` `↓` `Enter` | Move and pick in the model list |
| `Enter` | Send |
| `⌘C` `⌘V` `⌘X` `⌘A` | Edit text |
| `⌘M` | Minimize |
| `⌃⌘F` | Full screen |
| `⌘Q` | Quit |

## Troubleshooting

**"No … API key yet."** The provider you are on has no key. The model chip still
opens the list, and a provider whose key is stored can be picked from there, so the
quickest way out is to pick one of its models. Otherwise paste the key in Settings,
or export the variable in the shell you launched from. Sending only keeps the draft,
so nothing typed is lost while you fix it.

**401, 404, or "model not found".** Check the provider's key, endpoint and model.
A custom endpoint usually needs the `/v1` suffix; Anthropic speaks its own API.

**Ollama or custom says no model is chosen.** Ollama lists nothing until its
server answers: start it and pick a model from the composer. A custom endpoint
needs its model and its endpoint in Settings; the endpoint usually ends in `/v1`.

**`terminal` says a command is missing but your shell finds it.** A bundle started
from Finder gets launchd's `PATH`; the app adopts the login shell's environment at
startup, so log out and back in if the shell changed.
