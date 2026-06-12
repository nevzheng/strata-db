# Contributing

## Contents

- [How this project is run](#how-this-project-is-run)
- [Getting started](#getting-started)
- [Workflow](#workflow)
- [Branches](#branches)
- [Commits](#commits)
- [Pull requests](#pull-requests)
- [Code](#code)
- [Writing style](#writing-style)

## How this project is run

strata-db is my personal craft project for the time being. I maintain it solo and
make the final call on scope, design, and direction — a single vision keeps it coherent.

I also believe in learning in the open — teaching, sharing what I work through, and
learning from others. Engaging with people here is part of that, and something I
genuinely value.

That engagement is best-effort, though — my time is finite:

- I work on this by capacity and interest. Reviews and replies come when they come — no promises on turnaround.
- Issues and PRs are welcome. I may decline ones that don't fit where I'm taking it — that's about fit, not about you or your work.
- Before a sizable PR, open an issue so we can align on the approach first.

## Getting started

Rust workspace, 2024 edition. Hooks run via husky (`npm install` once).

```bash
cargo build
cargo test
cargo run -p spec-test -- spec-test/spec                                  # SQL spec suite
cargo run -p strata-server -- --listen 127.0.0.1:5433 --data-dir ./strata-data
cargo run -p strata-cli -- --port 5433                                    # or any pgwire client
```

Testing is Rust unit tests plus the sqllogictest spec suite — cover changes with both where it fits.

Architecture and crate layout live in the [README](README.md).

## Workflow

Match the process to the size of the change.

- **Small to medium** — bug fixes and focused improvements. Open a PR directly, following the branch and commit conventions below. Prefer to batch several small fixes into one coherent PR rather than a stream of tiny ones.
- **Large or structural** — new subsystems, on-disk format changes, anything that moves the design. Open an issue first with a design doc (or ADR), written in the issue or linked from it. We align on the approach before any code.

Design docs and ADRs live in issues, not in the repo.

I hold a high bar for design and engineering here. A strong proposal reasons from
the problem up, not from a solution backward.

### Design docs & ADRs

Both follow one structure, posted in the issue (or linked). I like the top-down
style of [Smart Brevity](https://www.axios.com/smart-brevity) — lead with the point,
then just enough to support it, not a word more.

- **Context & problem** — what exists, and what's wrong. Clear, clean, concise.
- **Options + recommendation** — the alternatives, a comparison **table** (each dimension tied to a requirement), and the one you recommend.
- **Scope & future work** — what's in, what's deferred, what it opens up later.
- **References** — papers, prior art, links.

Keep deep implementation detail in your own notes, not the doc. Be ready to share
or present it on request.

Start one with the **Design doc** or **ADR** issue template. For a worked example,
see [Beyond Inline Values](https://n8z.dev/posts/beyond-inline-values/).

<!-- TODO(nlz): write up thoughts on design process & documentation in the wiki (TBD) -->


## Branches

Branch names follow the pattern `type/short-description`:

```md
- feat/add-memtable
- fix/null-query-result
- refactor/split-query-engine
- chore/bump-tokio
- spike/duckdb-feasibility
```

A `spike` is a time-boxed investigation — the output is knowledge, not production code.

Include a ticket or issue ID when one exists:
`feat/ENG-42-add-memtable` or `fix/123-null-query-result`.

Use the same types as commits. All lowercase, hyphens between words.

## Commits

We follow [Conventional Commits](https://www.conventionalcommits.org/en/v1.0.0/).
Enforced locally via commitlint + husky.

Every commit should capture your thinking, not just your typing.
Even on branches that get squashed, well-structured commits help
you stay oriented, self-review, and debug.

A good commit answers some or all of:

- What's the goal of this change?
- Why is it needed?
- What approach are you taking?
- How are you validating it?

Not every commit needs all four. A small fix might just be
`fix(parser): handle null values`. A larger change deserves
a body that explains the reasoning.

### Commit Template

```md
type(scope): what changed

Why:

Changes:

Testing:
```

## Pull Requests

PRs are the primary unit of change. The PR description is the
comprehensive record — it consolidates the story your commits
tell into a single summary.

Commits get squashed on merge. The PR title becomes the commit
on main. Put your best summary there.

Use the PR template. Fill in what's relevant, skip what isn't.

## Code

Write clean, well-documented code. Explain the why, not the what.
Test your changes — unit tests at minimum, integration/e2e when
appropriate.

## Writing Style

These are recommendations, not rules.

Influenced by [Smart Brevity](https://www.axios.com/smart-brevity)
and general technical writing best practices.

- Active voice over passive ("flush the buffer" not "the buffer is flushed")
- Imperative mood for instructions ("add tests" not "you should add tests")
- Present tense ("this function returns" not "this function will return")
- Short sentences. Say it once.
- Plain language over jargon when possible
- Lead with the point. Context comes after, not before.
- Be specific. "Latency dropped 40ms" not "performance improved significantly."
- Avoid weasel words: "somewhat", "fairly", "arguably", "various."
- Cut filler. "In order to" → "to." "Due to the fact that" → "because."
- If a sentence works without a word, delete the word.
- Never write "just", "simply", "obviously", or "of course." If it were obvious, you wouldn't need to document it.
- Name things precisely. Pick one term for a concept and stick with it.
- Document decisions, not just outcomes. "We chose X over Y because Z" is more useful than "we use X."
- Write for a tired reader. That's you on a bad day.
