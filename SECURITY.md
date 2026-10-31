# Security policy

## Supported versions

Only the latest released version of `tickertape` and `tickertape-cli` receives fixes.

## Reporting a vulnerability

Please report security issues privately through GitHub's
[private vulnerability reporting](https://github.com/thomas0903/tickertape/security/advisories/new) for this
repository, not in a public issue. Include the version, platform and filesystem, and steps to reproduce (a small
journal directory that triggers the problem is ideal).

You should get an acknowledgement within a week. Once a fix is ready, it is released and the advisory published with
credit to the reporter unless you prefer otherwise.

## What counts

tickertape reads files that other processes write, so these are in scope:

- memory unsafety, a panic or an unbounded allocation in a reader, `tickertape verify`/`inspect`/`export`, or the
  writer's recovery, caused by the contents of journal files;
- `Reader::open` (the safe reader) behaving unsoundly whatever other processes do to the files;
- any way for recovery to delete or overwrite data instead of preserving it in `quarantine/`.

Known, documented limits are not vulnerabilities: `SIGBUS` when another process truncates a mapped file, the
`unsafe` contract of `Reader::open_mapped`, and the platform limits listed in `docs/crash-contract.md`.
