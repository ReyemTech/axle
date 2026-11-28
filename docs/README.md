# Axle documentation

Axle (Agent eXchange & Lease, CLI `axl`) is a self-hosted MCP server for coordinating coding
agents: durable messages, atomic task claims with renewable leases, and a web UI for the
human who runs them.

## Start here

- [Getting started](getting-started.md): install, connect a machine and hand out a first task.
- [CLI reference](cli.md): `install`, `login`, `logout`, `uninstall`, `status` and `service`.
- [Concepts](concepts.md): the model behind everything else. Read it once.

## Use

- [MCP tools](mcp-tools.md): the 17 tools an agent can call.
- [Web UI](web-ui.md): the board, Needs you, plan approval, the inbox and the admin pages.
- [Delivery stages](delivery.md): keep a task on the board until its pull request lands.
- Harnesses: [Claude Code](harnesses/claude-code.md), [Codex](harnesses/codex.md),
  [OpenCode](harnesses/opencode.md).

## Run

- [Configuration](configuration.md): environment variables, database URLs and UI sign-in.
- Deploy: [local](deploy/local.md), [Docker](deploy/docker.md),
  [Kubernetes with Helm](deploy/kubernetes.md).
- [Security](security.md): what Axle protects against and what it does not.
- [Troubleshooting](troubleshooting.md) and [FAQ](faq.md).

## Reference

- [HTTP API](http-api.md): `/v1`, `/healthz`, `/readyz` and the event stream.
- [Architecture](architecture.md): components, request flow, stores and the event bus.
- [Helm chart values](../deploy/helm/axle/README.md).

## Project

- [Roadmap](../ROADMAP.md) and [changelog](../CHANGELOG.md).
- [Contributing](../CONTRIBUTING.md), [code of conduct](../CODE_OF_CONDUCT.md) and
  [security policy](../SECURITY.md).
- [Releasing](releasing.md): how maintainers cut a release.

## Design records

The internal design documents Axle was built from. They record intent and decisions at the
time and are not kept in sync with the code; where they disagree with these guides, the
guides (and the code) win.

- [Agent coordination server: design](superpowers/specs/2026-11-27-agent-coordination-design.md)
- [Plan gate and multi-project tasks: design](superpowers/specs/2026-11-27-plan-gate-and-projects-design.md)
- [MVP implementation plan](superpowers/plans/2026-11-27-coord-mvp.md)
- [MVP follow-ups](superpowers/plans/2026-11-27-coord-mvp-followups.md)
