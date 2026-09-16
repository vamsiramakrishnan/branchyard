Branchyard
A Rust SDK for meta-harnesses that dynamically spawn, coordinate, and merge coding agents on servers.
Branchyard gives a parent harness control over other harnesses. It can discover work, create children, choose their tools and environments, share selected resources, and bring their changes back together.
The topology develops as the work unfolds. You define capabilities, budgets, and acceptance rules; the meta-harness decides how to organize the work.
Status: Design and implementation preparation. The interfaces and capabilities below describe the intended system, not a released SDK.
What Branchyard does
Dynamic delegation: Spawn children, assign work, exchange messages, and change dependencies at runtime.
Isolated execution: Run harnesses in server-side sandboxes with explicit compute, storage, and network limits.
Selective sharing: Give children private workspaces, read-only components, or explicitly coordinated shared resources.
Harness interoperability: Control existing coding agents through ACP and native interfaces.
Durable supervision: Track tasks, sessions, budgets, cancellation, and recoverable failures independently of connected clients.
Controlled integration: Collect candidate changes, validate them, and merge them according to an explicit policy.
How it works
A client submits a task to a meta-harness running on the server.
The meta-harness requests children as it discovers useful subtasks.
Branchyard checks authority and available budget, prepares workspaces, and starts the selected harnesses.
Children work within their granted resources and can request further delegation.
Results return as artifacts and candidate commits. Branchyard runs acceptance checks before promoting changes.
A task, a conversation, a sandbox, and a Git branch have separate identities. Forking a conversation does not automatically copy its filesystem or credentials.
Interfaces
Interface
Purpose
Rust SDK
Build meta-harnesses and server integrations
Server API
Submit commands, inspect state, and stream events
ACP drivers
Control harness sessions through a common protocol
Native drivers
Preserve harness-specific lifecycle and session controls
MCP tools
Let a harness request Branchyard operations such as spawning a child
ACP is the common integration path, with native drivers where they provide better control. Initial targets include Claude Code, Codex, Antigravity, Oh My Pi, DeepSeek Harness, Gemini CLI, and OpenCode. Each integration will be qualified against pinned versions and declared capabilities.
Server-first architecture
All agent execution happens on servers. Clients submit work and observe it; they do not host sandboxes.
The planned Rust control plane uses Tokio, Axum, and PostgreSQL, with separate interfaces for sandbox providers and harness drivers. Existing runtimes handle isolation. Warm capacity, prebuilt images, cached repository objects, and private writable layers are the intended path to fast startup.
Microsandbox's open runtime is the first sandbox-provider evaluation target. Branchyard's self-hosted architecture does not depend on access to a vendor's private-beta cloud service. Runtime capabilities and startup latency must be verified before being advertised.
Workspaces and merging
Children normally start from an exact code checkpoint in a private workspace. Sharing is explicit and scoped to a resource: source code, dependencies, artifacts, or a service.
Completed work becomes an integration proposal. The merge coordinator prepares a candidate, runs checks against that exact candidate, and promotes it only if the target has not changed. Conflicts return to a harness for resolution and another validation pass.
Shared writable workspaces require coordinated ownership. Filesystem snapshots and Git merges solve different problems; neither merges live process state or databases automatically.
Built on existing controls
We are selecting reusable controls from Scion, Herdr, and OpenRig: provisioning, session handling, configuration projection, and readiness checks. Vendored sources retain their licenses, exact revisions, and recorded modifications.
Warp informs process supervision; its AGPL application sources require separate license treatment. Replicas informs the server-workspace experience. We do not claim to vendor proprietary service implementations.
Implementation sequence
One complete task: Server API, one sandbox provider, one harness driver, durable events, and artifact capture.
Dynamic children: Runtime delegation, scoped capabilities, budget reservations, cancellation, and workspace branching.
Validated integration: Candidate checks, conflict handling, and conditional promotion.
Broader interoperability: Additional harness profiles, compatibility tests, warm pools, and measured concurrency improvements.
License
Proposed license for Branchyard-authored code: Apache-2.0. Third-party components retain their own licenses. Any AGPL source collection must be identified separately and is not implicitly covered by Branchyard's license.