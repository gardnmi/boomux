# Security Policy

## Supported Versions

Security fixes are provided for the latest published Boomux release. Upgrade to
the latest release before reporting an issue that may already be fixed.

## Reporting A Vulnerability

Report suspected vulnerabilities privately through
[GitHub Private Vulnerability Reporting](https://github.com/gardnmi/boomux/security/advisories/new).
Do not open a public issue for a suspected vulnerability.

Include the Boomux version, operating system, affected feature, reproduction
steps, and impact when known. Do not include terminal contents, environment
variables, credentials, SSH output, private paths, external session IDs, or
configuration contents unless they are essential and have been redacted.

You should receive an acknowledgement within seven days. Please allow time for
investigation and a coordinated release before publishing details.

## Security Boundaries

Writable web-terminal access is equivalent to shell access. Boomux's web
services bind to loopback; operators are responsible for authentication, TLS,
and access controls when publishing them through a private network layer.

Shells and coding-host integrations run with the current user's privileges and
are not sandboxed. See the [security and privacy notes](README.md#security-and-privacy)
and [mobile web contract](docs/mobile-web.md#security-and-privacy) for the
current boundaries.
