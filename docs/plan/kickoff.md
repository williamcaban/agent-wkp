# Kickoff: moving from design to implementation in Claude Code

## One-time setup (human)

```bash
git clone git@github.com:williamcaban/agent-wkp.git && cd agent-wkp
git tag v0-python main && git push origin v0-python     # freeze the Python implementation
git checkout v2-rust
rustup toolchain install stable && rustup component add clippy rustfmt
cargo install cargo-deny cargo-audit cargo-vet             # once per machine
claude                                                     # start Claude Code in the repo
```

Enable SSH commit signing on this machine so the M2 work is tested against real signed history:

```bash
git config gpg.format ssh
git config user.signingkey ~/.ssh/id_ed25519.pub
git config commit.gpgsign true
```

## Kickoff prompt (paste into Claude Code)

```
Read CLAUDE.md, docs/plan/milestones.md, and docs/design/wkp-hub-design-v0.1.md sections 3, 4 and 5.

We are starting milestone M0. Enter plan mode and produce a plan for issue #<n> ("Scaffold the Cargo workspace") that follows the layout in design section 3.3 and the release profile in section 9.6. The plan must list every file you will create, every crate you will add with a one-line justification, and the commands you will run to verify. Do not add any dependency that reimplements git, SQLite or OpenSSH functionality.

After I approve the plan, implement it, run the commands in the CLAUDE.md "Commands" section, and open a PR against v2-rust with the issue's acceptance criteria checked off in the description.
```

## Working rhythm

- One issue per Claude Code session. Start every session with `/clear` and the issue number; the SessionStart hook (once M1 lands) loads the store's Tier 0, which includes this repo's decisions.
- Use plan mode for anything touching `wkp-crypto`, `wkp-git`, `wkp-hub` or CI. Review the plan before code exists.
- When Claude Code proposes a deviation from the design, ask it to write the ADR first, then decide.
- Merge only from PRs with CI green. Never let the agent push to `v2-rust` directly.
- After M1's exit criterion holds, point the repo's own `.wkp` store at the design doc, ADRs and milestone file so later sessions start with them in Tier 0. The project dogfoods itself from that point.

## Using other harnesses as maintainers

The same issues and CLAUDE.md work for OpenCode (reads `AGENTS.md`; add a one-line pointer to `CLAUDE.md` there when M1 rewrites it), Hermes Agent and dsh. The rule set is harness-independent: an issue with acceptance criteria in, a PR with CI green out, a human co-sign on CODEOWNERS paths.
