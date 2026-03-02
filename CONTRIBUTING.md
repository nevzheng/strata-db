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
```
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
