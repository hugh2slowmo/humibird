# Security

humibird runs tools on your machine. The approval gate and the workspace
boundary are product requirements, not hardening — but neither is a sandbox:
an approved `terminal` command runs with your privileges, and the agent can read
anything you can read inside the workspace you gave it.

## Reporting a vulnerability

Please do not open a public issue. Use this repository's private advisory form
(GitHub: **Security → Report a vulnerability**) and include:

- the commit or build you ran,
- the shortest steps that reproduce it — a workspace and a prompt are enough to
  start,
- what an attacker gains.

There are no supported releases yet; fixes land on the default branch.

## In scope

- **Leaving the workspace.** A file tool reaching a path outside it, following
  a symlink out, or accepting `/`, your home directory, or the profile tree as a
  workspace.
- **Running without asking.** A mutating tool or `terminal` call that executes
  without the approval it requires, or a denial that the model reads as success.
- **Reading secrets.** Reaching the profile's `.env` or raw session database
  through a file or terminal tool, or writing a key into the transcript, a log,
  or a request it does not belong to. The `session_search` tool intentionally
  exposes bounded user/assistant conversation excerpts from the current profile.
  It defaults to the current workspace; other workspaces require an explicit
  `all_workspaces` argument. Access to other profiles, the active session,
  subagent sessions, attachments, or tool output through this tool is a bug.
- **Injection that acts on its own.** Fetched content is labelled as data and
  every mutating call still waits for a person; a case where injected text gets
  a tool to change something without a prompt is a bug.

## Out of scope

- A tool doing what you approved. An approved command is your command.
- An attacker who already runs code as you, or can read the profile directory.
- How a provider stores or uses your data, and what a request costs.
- Denial of service against your own machine by commands you approved.

## Keys

Keys are plain text in the profile's `.env`, with `0600` permissions. That is
deliberate — there is no keychain and no account — so the file's permissions are
the whole of the protection. If someone else can read it, rotate the key at the
provider.
