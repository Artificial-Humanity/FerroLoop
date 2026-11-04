# Recorded surfaces for whole-slice proof tests

Captured on 2026-11-04 UTC from locally installed command-line programs, without model
requests. Trailing blank lines in help were normalized. These fixtures are snapshots, not a live version check, a capability proof,
or evidence of a reviewed production bound. A vendor release requires a deliberate
recapture and review; tests alone do not detect a changed binary on a user's machine.

- `claude-2.1.317-help.txt`: `~/.local/bin/claude --help`; `--version` reported
  `2.1.317 (Claude Code)`; launcher/binary SHA-256
  `eb224419e344575faa4a1f39b290b649425a10cd5ee2ef1d003480cd9ae57eb0`.
- `muse-1.4.14-help.txt`, `muse-1.4.14-exec-help.txt`:
  `MUSE_NO_AUTO_UPDATE=1 ~/.local/bin/muse --help` and `muse exec --help`;
  `--version` reported `Muse Code 1.4.14 (1.4.14-R7309.1)`. The launcher SHA-256
  was `c6db294799a190ca380da274beb3b9c0e160e0da9681a3d364ce8b0e5fa3a4bc`;
  it is a shell launcher, not the underlying runtime digest.
- `codex-0.166.0-turn-start.json`: `~/.local/bin/codex app-server
  generate-json-schema --out <temp-dir>`, copied the generated
  `v2/TurnStartParams.json`; `--version` reported `codex-cli 0.166.0`.
  Launcher/binary SHA-256 `b8a11fedf24b2dfb59a70c9e52b030f9a508dcd49cd2dd0d41956f4c785156b8`.

Muse's captured `exec --help` advertises `--max-model-steps` and
`--max-tool-output-bytes`, but neither this help nor parent help advertises
`--max-turns`. The active L2 tests pin the two observed flags and reject a candidate
requiring `--max-turns` as unproven because that knob is missing. Matching help
alone does not establish a proof or authorize production work. Codex's captured
turn schema exposes `input` and `threadId` but no `maxOutputTokens` or `maxSteps`
property. None of these observations establishes whether internal calls or spend
are bounded.
