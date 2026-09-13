# Documentation Guide

Start with the guide for your task. Protocol contracts and dated validation
records are separate from everyday usage instructions.

## Use Boomux Desktop

| Task | Guide |
| --- | --- |
| Install and launch | [Desktop quick start](../desktop/README.md#install-release-builds) |
| Move, resize, and navigate | [Controls](../desktop/README.md#controls) |
| Manage projects and preferences | [Settings](../desktop/README.md#settings) |
| Connect a remote machine | [Remotes](../desktop/README.md#remotes) |
| Understand Git status | [Git overview](../desktop/README.md#git-overview) and [status semantics](desktop/git-panel.md#status-semantics) |
| Select Kiro v2 or v3 | [Kiro integrations and lifecycle limits](kiro.md) |
| Manage harness integrations | [Automatic integration management](install.md#automatic-integration-management) |
| Update or migrate | [Desktop release guide](desktop/releases.md#distribution-and-installation) |
| Remove Desktop | [Desktop uninstall](desktop/releases.md#uninstall) |
| Remove a standalone or remote installation | [Uninstall contract](uninstall.md) |
| Use the phone/web dashboard | [Mobile web](mobile-web.md) — review its security section first |

## Develop Or Automate

| Task | Reference |
| --- | --- |
| Build, test, and contribute | [Development guide](../DEVELOPMENT.md) |
| Understand resource ownership | [Product glossary](../CONTEXT.md) |
| Find the owning backend module | [Architecture module map](architecture.md#module-ownership) |
| Work on rendering or input | [Desktop architecture](desktop/architecture.md) |
| Measure performance | [Desktop performance](desktop/performance.md) |
| Write CLI automation | [JSON contract](cli-json.md) |
| Consume changes | [Event stream](event-stream.md) |
| Change daemon replacement | [Live PTY handoff](live-pty-handoff.md) |
| Work on SSH federation | [Remote Node contract](remote-nodes.md) |
| Review host compatibility evidence | [Lifecycle validation](lifecycle-validation.md) |

## Which Document Wins?

1. [CONTEXT.md](../CONTEXT.md) defines product terminology.
2. Architecture and named contracts define implementation boundaries and guarantees.
3. Source and compatibility tests define exact protocol and persistence versions.
4. [ADRs](adr/) explain accepted decisions; validation records describe tested versions.
5. Roadmaps and brainstorms describe future or historical ideas, not shipped behavior.

See [SECURITY.md](../SECURITY.md) for private vulnerability reporting.
