Closes #

## Acceptance criteria (copied from the issue, each checked)

- [ ]

## New dependencies (name, version, one-line justification, or "none")

none

## Commands run locally (paste the tail of the output)

```
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo deny check && cargo audit
cargo bench -p wkp-core   (only if the hot path changed)
```

## Design impact

- Design sections touched:
- Deviation from the design: none | ADR-nnnn
- Touches a CODEOWNERS path: no | yes (human co-sign requested)

## Produced by

Harness and model, or "human".
