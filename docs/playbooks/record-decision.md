# Playbook: record a decision (ADR)

Write an ADR when you choose a byte format, a protocol, a durability guarantee, a public API shape, a dependency, a
platform behaviour or a test strategy, or when you reject an obvious alternative.

1. Copy `docs/decisions/_template.md` to `docs/decisions/NNNN-kebab-title.md`, using the next free number.
2. Keep it to about one page. Give the context, the options, the decision and its consequences. Link the spec
   section (`docs/format.md`, `docs/crash-contract.md`) it affects, and update that spec in the same change.
3. Use status `proposed` while the owner still has to agree (e.g. during the M1 approval gate), and `accepted` once
   they have, or once you have decided something within your remit.
4. When you supersede an ADR, set the old one's status to `superseded by NNNN`. Don't change anything else in it.
5. Commit: `docs: ADR NNNN <title>`.
