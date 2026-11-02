# Agent instructions

Read [HANDOFF.md](HANDOFF.md) before making changes. It has the rules (bot accounts only, native
egui stack, performance budgets, small self-merged PRs), exact build/test/bench commands, gotchas
and the prioritised next steps.

Quick checks before every push:

```sh
cargo test --workspace
python3 bench/run.py --all --check   # when a change could affect performance (needs a release build)
```
