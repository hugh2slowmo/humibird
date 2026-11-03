# AGENTS.md

humibird is a GPUI desktop agent.

## Working agreement

### Smallest verifiable unit, then commit

- Before writing code, state the unit and how it will be verified. If you cannot
  name the verification, the unit is too big or too vague.
- A unit is the smallest change that produces an observable, testable result:
  one function plus its test, one schema plus its migration test, one UI
  behavior plus a GPUI test.
- Run the verification, then commit immediately. Do not accumulate units.
- **One commit = one unit.** Never mix a refactor, a behavior change, and a doc
  update in the same commit. If a refactor is a prerequisite, commit it
  separately, verified on its own.
- Commit subject: imperative, prefixed with the crate or area
  (`humibird_store: reject out-of-order message seqs`). The body says what was
  verified and how to reproduce it.
- Never commit unverified work as done. If something cannot be verified yet, say
  so in the commit body and keep it isolated.

### Delegation

Use subagents when work splits into independent pieces. Keep anything sequential
or dependent on whole-project judgment in the main thread.

**Good delegation targets**

- Independent research across disjoint directories.
- Mechanical edits with disjoint write scopes.
- Long builds or test runs where only a summary (plus the relevant failing
  lines) is needed.

**Do not delegate**

- The turn loop, request assembly, prompt-caching invariants, or memory
  semantics. These are the parts where a locally reasonable change violates a
  global rule.
- Cross-crate refactors.
- Debugging, where each step depends on the result of the previous one.

**Every delegation prompt must include**

1. The goal and the exact expected output.
2. Allowed paths (specific directories or files) and off-limits paths.
3. The relevant invariant excerpts, and existing API signatures it needs.
4. The command that verifies the result.
5. An explicit instruction to stay inside the assigned paths.

**Research tasks run under hard file boundaries.** No `find /`, no
repository-root recursive `grep`, no scanning the home directory. Start from the
paths you were given; if the answer is not there, report that instead of widening
the search.

Subagents do not share the main thread's conversation and do not know humibird's
invariants. Assume they will do the locally correct thing that violates a global
rule unless you state the rule.

When running several in parallel, assign disjoint write scopes. The main agent
reviews the diff and owns the commit.

### Docs are contracts

If a change contradicts a doc, either update the doc
in the same commit (and explain why the old rule no longer holds) or do not
make the change.

### Verification defaults

Once the Cargo workspace exists, the per-unit check is:

```sh
cargo fmt --check
cargo clippy --all-targets -- -D warnings
cargo test -p <crate>
```

UI work adds a GPUI test or an explicit manual check described in the commit
body. A build alone is not verification.
