# scommit

Smart, low-friction `git commit` helper:

- Stages everything (`git add -A`) unless you opt out.
- Builds a concise commit subject/body from the staged diff (files, additions, deletions, categories).
- Commits, rebases on top of upstream when behind, then pushes.
- Prints what it would do in `--dry-run` mode.

## Install

```bash
cargo install --path .
```

## Usage

From any git repo with changes:

```bash
scommit             # stage, generate message, commit, pull --rebase if needed, push
scommit --dry-run   # show subject/body and actions only
scommit --no-stage  # use already-staged changes
scommit --no-push   # commit only
scommit --skip-pull # don't rebase even if behind upstream
scommit -m "msg"    # force subject; auto body still included
scommit --no-ai     # turn off AI generation even when OPENAI_API_KEY is set
scommit --model gpt-4o # override OpenAI model (default: gpt-4o-mini or $SCOMMIT_MODEL)
```

## How messages are built

- Categorizes files (docs/tests/config/code/other) and totals additions/deletions.
- Chooses a safe prefix (`docs`, `test`, `chore`, `feat`, or `refactor`) based on the staged diff.
- Subject highlights the most-changed files (max 72 chars).
- Body lists up to 12 files with +/– counts and a generated timestamp.

If you want full control over the subject line, pass `-m "your title"`; the auto body remains to keep the context.

## AI-powered commit messages

Set `OPENAI_API_KEY` in your shell to let scommit ask OpenAI's Chat Completions API for a repo-aware subject/body. The tool:

- Feeds staged file changes (+/– counts & categories) plus the last few commit subjects to the model, so it can stay consistent with repo voice.
- Returns JSON (`{subject, body}`) and falls back to the heuristic generator on any error.
- Respects `--no-ai` to disable and `--model`/`SCOMMIT_MODEL` to pick a model (default: `gpt-4o-mini`).
