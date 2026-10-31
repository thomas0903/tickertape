# 0012: Keep AI-agent tooling out of the repository

- **Status:** accepted (owner decision)
- **Date:** 2026-11-01

## Context

ADR 0001 put the instructions and skills that coding agents use (`AGENTS.md`/`CLAUDE.md` per directory, vendored
skills under `.agents/`, `.claude/` settings, an agent status dashboard and a delegation playbook) into the
repository. They help when working on tickertape, but they are not part of the product, and the repository will be
public.

## Decision

These files are no longer tracked. They are listed in `.gitignore` and kept locally, outside version control of this
repository. The public repository contains only the library, the CLI, tests, benchmarks, examples, CI and
user/contributor documentation. The parts of ADR 0001 about agent instruction files and the agent dashboard are
superseded; its workspace layout and other decisions stand.

## Consequences

- Contributor-facing guidance lives in `CONTRIBUTING.md` and `docs/playbooks/`.
- Earlier commits (up to and including the `v0.1.0` tag) still contain those files; removing them from history
  would need a history rewrite, which is a separate decision.
- The crate manifests keep excluding `AGENTS.md`/`CLAUDE.md`, so a local copy can never end up in a published crate.
