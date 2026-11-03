# Contributing

Thanks for taking the time. humibird is early, and the most useful contributions
are the ones that make what already exists more trustworthy:

- **Bug fixes with a reproduction** — the steps, the log line, and where possible
  a test that fails before the fix.
- **Reach at the edges** — more providers, more model knowledge, settings that
  cover a real workflow.
- **Docs that are wrong or missing**, told from the reader's side.

A large feature is worth a short design discussion before the code. One piece of
the design is not built yet — scheduled jobs.

## Docs are contracts

If a change contradicts a doc, update the doc in the same commit and say why the
old rule no longer holds.

## Small units

A unit is the smallest change with an observable result: a function and its test,
a schema and its migration test, a UI behaviour and a GPUI test. Make it, verify
it, commit it — then start the next one.

- One commit, one unit: do not mix a refactor, a behaviour change and a doc
  update. A prerequisite refactor is its own commit.
- Subject: imperative, prefixed with the crate or area when one is the scope —
  `humibird_store: reject out-of-order message seqs`.
- Body: what you verified, and how to reproduce it. Do not commit unverified work
  as done; say plainly what could not be checked.

## Verify

```sh
cargo fmt --all --check
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline
```

A build is not verification: UI work needs a GPUI test or a manual check written
in the commit body, and a bug fix needs a test that fails on the commit before it.
Tests assert behaviour a caller or a user can observe, not the shape of the code.

## Code

Ordinary Rust, with three habits worth naming:

- Prefer the existing dependency and the existing pattern; a new crate or
  dependency needs a reason in the commit message.
- No `unwrap()` in library code — propagate with `?`, or `.log_err()` when
  ignoring is deliberate. Never silently discard a fallible operation.
- Comments explain *why*. A comment that restates the line below it is noise.

## Pull requests

A clear, imperative title, no `fix:` / `feat:` prefixes, no trailing period. Keep
the diff to the unit you described, say what you ran and what you could not run,
and rebase onto the default branch before merging — a squash from a stale branch
silently drops newer fixes.

## Reporting bugs

Say what you did, what happened, and what you expected. Attach
`logs/humibird.log` from the profile directory (`~/.humibird/profiles/default/`);
it is the only record a bundle started from Finder leaves. **Remove API keys**
from anything you paste.

## Security

An approved `terminal` command runs as you, and the workspace boundary is a
requirement rather than hardening, so a bypass is a security bug.
[SECURITY.md](SECURITY.md) says what is in scope and how to report it privately.
Do not open a public issue for a vulnerability with a working reproduction.
