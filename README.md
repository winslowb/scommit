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
```

## How messages are built

- Categorizes files (docs/tests/config/code/other) and totals additions/deletions.
- Chooses a safe prefix (`docs`, `test`, `chore`, `feat`, or `refactor`) based on the staged diff.
- Subject highlights the most-changed files (max 72 chars).
- Body lists up to 12 files with +/– counts and a generated timestamp.

If you want full control over the subject line, pass `-m "your title"`; the auto body remains to keep the context.

