# CLAUDE.md

## Git & Branching
- **Do NOT create new branches without asking me first.** Work on the current branch by default.
- Keep new branches to a minimum. If one seems truly necessary, explain why and wait for approval.
- **Do NOT merge on your own.** No auto-merging, no rebasing onto other branches unless I explicitly ask.
- Edit exactly where I tell you to edit. Don't move changes to other branches or files "for cleanliness."

## After Finishing a Task
- When a task is done, **build and install it directly on the current machine/device** so I can test it. Don't ask, just do it.
- Install command: `<FILL IN: e.g. flutter install / adb install -r build/app.apk / npm run build && npm link>`
- If the build or install fails, fix it and try again before reporting back. Only report back if you're truly stuck.

## Model Usage (Subagents)
When delegating to subagents, pick the model that fits the work:
- **Opus**: planning, architecture decisions, complex reasoning, hard or core coding, deep investigation and debugging of root causes.
- **Sonnet**: the default for most remaining work, such as feature implementation, ordinary refactors, writing tests, and moderate fixes.
- **Haiku**: repetitive, simple tasks where reporting is the main output, such as listing files, searching for usages, collecting info, running loops of checks, and summarizing results.

Don't use Opus for work Haiku or Sonnet can handle. Don't use Haiku for anything that needs judgment.

## Decision-Making: Ask As Little As Possible
- **Minimize questions.** When facing a choice (up to ~4 options), if one option has **>80% confidence** of being right, choose it and proceed without asking.
- Log every such decision in `IMPLEMENTATION_DECISION.md` (create it if missing). Append only, newest at the bottom, using this format:

  ### [YYYY-MM-DD HH:mm] <Short title>
  - **Context:** what needed deciding
  - **Options:** A / B / C / D
  - **Chosen:** X (confidence ~NN%)
  - **Why:** 1–2 sentences
  - **Revert if:** what would make this the wrong call

- Ask me **only** when confidence is ≤80%, OR the action is destructive or irreversible (deleting data, force-push, dropping DB tables, changing secrets or production config). Those still require my approval regardless of confidence.
