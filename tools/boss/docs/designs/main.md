# Boss: High-Level Design

## Overview

Boss is a tool for managing and automating multiple coding agents. It supports
both local agents and cloud-hosted agents, and can be used interactively or
for automation.

The core metaphor is that the human is the **boss** of a team. The team is
autonomous: agents work independently on tasks, make decisions, and produce
results. The human stays in the loop to provide direction, adjust course, and
give feedback -- but doesn't micromanage the work.

## Core Concepts

### Team

A **team** is a collection of agents working together under the direction of a
human. A team is scoped to a project or workspace. The human boss can spin up
agents, assign work, monitor progress, and intervene when needed.

### Agents

An **agent** is an autonomous coding unit that can read and modify code, run
commands, and interact with tools. Agents can be:

- **Local**: running on the same machine as the boss engine (e.g. a CLI agent
  process).
- **Cloud**: running remotely and communicating over the network.

Boss interacts with agents via the [Agent Client Protocol (ACP)][acp], an open
standard for communication between editors/tools and coding agents. ACP was
originally created by Zed and has broad adoption across editors (Zed, JetBrains,
Neovim, Emacs) and agents (Claude Code, Codex CLI, Gemini CLI, and others).

Using ACP means boss is not tied to any specific agent implementation. Any
ACP-compatible agent can be added to a team.

[acp]: https://agentclientprotocol.com

### Tasks

A **task** is a unit of work assigned to an agent. Tasks can range from small
(fix a bug, write a test) to large (implement a feature, refactor a module).
The boss engine tracks task state and the human can monitor progress, review
results, and provide feedback.

### Human-in-the-Loop

The human boss operates at a higher level than the agents:

- **Direction**: defining what needs to be done, setting priorities.
- **Course correction**: noticing when an agent is going down the wrong path and
  redirecting.
- **Feedback**: reviewing agent output, approving or requesting changes.
- **Coordination**: managing dependencies between agents' work, resolving
  conflicts.

The system is designed so the human doesn't need to be constantly watching. Agents
work autonomously and surface results, blockers, and decisions that need human
input.

## Architecture

Boss is split into two layers:

```
┌─────────────────────────────┐
│         Frontend            │
│   (thin, platform-native)   │
├─────────────────────────────┤
│         Engine              │
│   (Rust service, core logic)│
├─────────────────────────────┤
│      Agent Client Protocol  │
├──────────┬──────────────────┤
│  Agent 1 │  Agent 2 │ ...   │
└──────────┴──────────────────┘
```

### Engine

The engine is a Rust service that contains all core logic:

- Agent lifecycle management (spawn, monitor, stop).
- Task management (create, assign, track, complete).
- ACP client implementation for communicating with agents.
- State persistence.
- API surface for frontends to consume.

The engine runs as a local service. Frontends connect to it; it connects out to
agents.

### Frontend

The frontend is deliberately thin. It presents the human boss with visibility
into the team and controls for directing work, but delegates all logic to the
engine.

The initial frontend will be a **native macOS app**. The architecture ensures
the frontend is a thin layer over the engine API, making it straightforward to
build frontends for other platforms in the future (web, Linux, other native
platforms).

The frontend provides:

- Team overview: which agents are active, what they're working on.
- Task management: create, prioritize, assign, review.
- Agent output: streaming view of what each agent is doing.
- Intervention controls: pause, redirect, provide feedback to agents.

### Agent Authentication

Boss should support ACP adapters that authenticate in different ways. For the
Claude Code ACP adapter, the design should allow both:

- **API key auth**: `ANTHROPIC_API_KEY` is present and passed through to the
  adapter process.
- **Claude Code login auth**: no API key is required if the local Claude Code
  CLI is already authenticated (for example via `claude /login` with stored
  local credentials).

For PoC ergonomics, the engine should not hard-require an API key at startup.
Instead, it should:

- pass `ANTHROPIC_API_KEY` only when provided,
- surface ACP auth-required responses clearly to the frontend,
- expose adapter-provided auth methods/metadata so the frontend can guide the
  user through login when needed.

## Modes of Operation

### Interactive

A human uses the frontend to manage agents in real time. This is the primary
mode -- the boss is actively directing and monitoring the team.

### Automation

Boss can also be driven programmatically. Tasks can be created and agents
managed via the engine API without a frontend. This enables:

- CI/CD integration: kick off agent work as part of a pipeline.
- Scripted workflows: define a sequence of tasks and let the team execute.
- Scheduled work: run agents on a schedule for maintenance tasks.

## Safety Model

For the initial PoC, boss prioritizes end-to-end architecture validation over a
strict sandbox. The engine may execute ACP file system and terminal requests
with normal local user permissions.

This is intentional for speed in the PoC. A production-ready version should add
explicit safety boundaries such as workspace scoping, allow/deny policy,
approval defaults, and audit logging.

## Future Considerations

- **Additional frontends**: web UI, Linux native, TUI.
- **Agent discovery**: integration with the ACP Registry for finding and
  installing agents.
- **Multi-project support**: managing teams across multiple codebases.
- **Remote management**: managing agents working across multiple remote
  machines from a single boss instance.
- **Agent-to-agent coordination**: letting agents communicate directly when
  appropriate, reducing the need for human routing.
- **Protocol hardening**: versioned/stable engine-frontend schema for
  multi-client compatibility.
