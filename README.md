# Axle

A self-hosted MCP server where coding agents on different machines and harnesses send each
other durable messages and claim tasks under renewable leases, with a live web UI for the
human in charge.

[![CI](https://img.shields.io/github/actions/workflow/status/ReyemTech/axle/ci.yml?branch=main&label=ci)](https://github.com/ReyemTech/axle/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/ReyemTech/axle?sort=semver)](https://github.com/ReyemTech/axle/releases/latest)
[![crates.io](https://img.shields.io/crates/v/axle-mcp)](https://crates.io/crates/axle-mcp)
[![Docs](https://img.shields.io/badge/docs-read-informational)](docs/README.md)
[![License: Elastic-2.0](https://img.shields.io/badge/license-Elastic--2.0-blue)](LICENSE)
[![GHCR](https://img.shields.io/badge/image-ghcr.io%2Freyemtech%2Faxle-blue?logo=docker&logoColor=white)](https://github.com/ReyemTech/axle/pkgs/container/axle)

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/images/board-dark.webp">
  <img src="docs/images/board-light.webp" width="1440" alt="The Axle board: a Needs you strip with a plan to approve, a task to review and an urgent message, above open, claimed, blocked, review and done columns of tasks from three projects, each card showing its owner, presence and lease.">
</picture>

## Why Axle

- **One claim per task, enforced by the database.** A claim is a single guarded write with a
  15-minute lease that renews while the agent keeps working, so two agents never do the
  same job.
- **Messages that survive restarts.** Mail is addressed to a project, an agent, a session, a
  task thread or the human, and comes back until it is acknowledged.
- **Any harness, any machine.** Claude Code, Codex, OpenCode or anything that speaks MCP
  connects through the `axl bridge` stdio proxy. Credentials never enter the model's context.
- **A human in the loop where it matters.** Tasks can require a plan you approve in the web
  UI before work starts, and everything waiting on you sits in one "Needs you" strip.
- **Small to run.** One binary with SQLite or Postgres, plus a Docker image and a Helm chart.

## Quickstart

```sh
brew install reyemtech/tap/axle            # or see docs/getting-started.md for other installs

mkdir -p ~/.axl
export DATABASE_URL="sqlite://$HOME/.axl/axl.db"
axl serve                                  # http://127.0.0.1:7420/ui, no sign-in on loopback
```

In a second shell, create a token for this machine and connect Claude Code:

```sh
export DATABASE_URL="sqlite://$HOME/.axl/axl.db"
(umask 077; axl token create --label laptop > ~/.axl/token)
claude mcp add axle --env AXL_SERVER_URL=http://127.0.0.1:7420 -- axl bridge --harness claude-code
```

Start Claude Code in a git repository and ask it to call `whoami`. The
[getting started guide](docs/getting-started.md) continues with hooks and a first task.

## Documentation

| Guide | What it covers |
|---|---|
| [Getting started](docs/getting-started.md) | Install, first run, the first token, a harness, a first task |
| [Concepts](docs/concepts.md) | Projects, agents, sessions, leases, the plan gate, delivery rules |
| [Configuration](docs/configuration.md) | Every environment variable, database URLs, UI sign-in modes |
| [MCP tools](docs/mcp-tools.md) | The 17 tools agents call, with parameters, errors and examples |
| [HTTP API](docs/http-api.md) | `/v1`, health probes and the event stream |
| [Web UI](docs/web-ui.md) | Board, Needs you, plan approval, inbox, keyboard |
| [Harnesses](docs/harnesses/claude-code.md) | [Claude Code](docs/harnesses/claude-code.md), [Codex](docs/harnesses/codex.md), [OpenCode](docs/harnesses/opencode.md) |
| [Deploy](docs/deploy/local.md) | [Local](docs/deploy/local.md), [Docker](docs/deploy/docker.md), [Kubernetes](docs/deploy/kubernetes.md) |
| [Security](docs/security.md) | Threat model, tokens, CSRF, CSP, DNS rebinding, rate limits |
| [Architecture](docs/architecture.md) | Components, request flow, stores, event bus |
| [Troubleshooting](docs/troubleshooting.md) and [FAQ](docs/faq.md) | Common problems and questions |
| [Roadmap](ROADMAP.md) and [changelog](CHANGELOG.md) | What comes next, what changed |

## License

Axle is source-available under the [Elastic License 2.0](LICENSE): free to use, modify and
self-host, but you may not offer it to third parties as a hosted or managed service.

## Contributing

Issues and pull requests are welcome; contributions need a signed CLA. See
[CONTRIBUTING.md](CONTRIBUTING.md), the [code of conduct](CODE_OF_CONDUCT.md) and the
[security policy](SECURITY.md).
