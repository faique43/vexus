# Security Policy

## Supported versions

vexus is pre-1.0. Only the latest release receives fixes.

| Version | Supported |
| --- | --- |
| 0.1.x | yes |
| < 0.1 | no |

## Reporting a vulnerability

Please report privately, not in a public issue.

Use GitHub's private vulnerability reporting: go to the
[Security tab](https://github.com/faique43/vexus/security/advisories/new) and
open a draft advisory. It is visible only to the maintainers until a fix ships.

Include what you can: the version (`vexus --version`), your platform, and the
smallest reproduction you have. You will get an acknowledgement within a week.

## What is in scope

vexus reads a repository, writes an index under `.vexus/`, downloads a model
over HTTPS on first run, and speaks MCP over stdio to a local client. Things
worth reporting:

- Path traversal that lets a tool read files outside the indexed repository.
  `open` is meant to refuse this; a bypass is a real bug.
- Anything that causes the model download to accept a file failing its pinned
  sha256, or that lets the pinned revision be substituted.
- A crafted repository that causes memory unsafety during parsing or indexing.
- Index or lock handling that lets one repository's `serve` read or corrupt
  another's data.

## What is not in scope

- **Indexed content is trusted.** vexus returns verbatim source from the
  repository you point it at. If you index a hostile repository, its contents
  reach your agent. That is what the tool does.
- Denial of service from pathological input (a multi-gigabyte single-line
  file, a pathologically deep call graph). These are bugs worth filing
  publicly, just not security ones.
- Missing hardening on a Windows or Intel macOS build. Neither platform is
  supported; see [CONTRIBUTING.md](CONTRIBUTING.md).
