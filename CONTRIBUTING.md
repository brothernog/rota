# Contributing

## Rule #1: nothing from any game

No game files, executables, shaders (in any form, including their names),
textures, audio, screenshots or logs that quote them. Measurements go in as
numbers only: fps, memory, error codes, counts. See
[docs/asset-policy.md](docs/asset-policy.md). Do not name games, publishers
or their staff in code, comments, commits or pull requests; say "the title".

The pre-commit hook enforces the asset policy. Install it once:

    cargo xtask install-hooks

## Code from other projects

GPL-3.0, MIT and Apache-2.0 code may come in. Keep its copyright lines, and
name the source repository and commit in the commit message. No GPL-2.0-only
code and nothing from repositories without a licence.

## Commits and pull requests

- English, imperative, prefixed with the area: `gpu: cap texture uploads per frame`.
- No AI tool attribution lines (`Co-Authored-By:` an AI, "Generated with ...").
  The commit-msg hook rejects them.
- If AI tools helped, the pull request body ends with `AI-assisted.`
  and a comment ends with `This comment was AI-assisted.`
- Before opening a pull request: `cargo test --workspace --locked` and
  `cargo clippy --workspace --all-targets --locked`, with no new warnings in the code you
  touch (the imported code still has some; fixing them is welcome).
