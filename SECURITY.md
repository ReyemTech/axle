# Security policy

## Reporting a vulnerability

Please do not open a public issue for a security problem. Report it privately through
GitHub Security Advisories:

**<https://github.com/ReyemTech/axle/security/advisories/new>**

Include what you found, how to reproduce it, the Axle version (`axl --version`) and how it
is deployed (local, Docker, Helm; SQLite or Postgres). Remove real tokens and secrets from
logs before you send them.

What to expect:

- An acknowledgement within 3 business days.
- An assessment and, if confirmed, a plan and a target date within 10 business days.
- Credit in the advisory and the changelog, unless you prefer otherwise.

Please give us a reasonable time to release a fix before disclosing publicly.

## Supported versions

Axle is pre-1.0. Security fixes go into the latest release only.

| Version | Supported |
|---|---|
| Latest 0.x release | Yes |
| Older releases | No; upgrade to the latest |

## Scope

The threat model, the defences Axle provides and what is out of scope are described in
[docs/security.md](docs/security.md). Reports about behavior listed there as out of scope
(for example, a valid token reading every project) are design decisions rather than
vulnerabilities, but we are glad to discuss them in an issue.
