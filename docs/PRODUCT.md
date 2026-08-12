# Axocoatl product model

This document is the contributor-facing source of truth for what Axocoatl is as a product.
Implementation remains the final authority for what ships. `BRAND.md` defines how the
product speaks and looks; `ARCHITECTURE.md` explains the runtime beneath it.

## One product

Axocoatl is a local-first coding workbench backed by a durable multi-agent runtime. It is
not a collection of dashboards for runtime subsystems. The runtime, actor model, memory,
lattice, isolation, tools, MCP, and automation machinery are the engine. The browser app
at `/` is how a person uses that engine.

A folder-anchored session is the unit of work. Its chat is the permanent spine. Files,
editor, terminal, browser, activity, attempt state, comparison, git, and agent graph open
around that session rather than replacing it.

## The primary loop

1. Start Axocoatl and open the local app.
2. Resume the last session or authorize a project directory and start a session.
3. Ask for one solution, or turn on **Explore several ways** and choose the agent and model
   for each attempt.
4. Watch attempts execute in isolated working copies. Running, blocked, failed, cancelled,
   and complete states remain visible even when switching sessions.
5. Compare **Outcome** and **Route**, inspect the actual diff and cost, run the repository's
   **Checks**, and use **Judge** when useful.
6. **Keep this one**. The selected changes return to the session checkout and the other
   attempts are cleaned up.
7. Review **Last turn** through git, stage or discard deliberately, and commit.

This loop is the product differentiator. A single answer remains the simple path; parallel
attempts add confidence when the task merits them.

## Information architecture

- **Left rail:** authorized workspaces and their sessions. A session row may show live
  attempt state. The rail is not a list of features.
- **Main area:** the active session chat, always.
- **Right dock:** state watched during execution, including attempt roster, plan, and cost.
- **Bottom dock:** terminal.
- **Over chat:** focused surfaces opened for review, including files, editor, diff,
  comparison, and trajectories.
- **Settings:** Agents, Skills, MCP servers, and Automations.

The app should resume the last active session. A Finder-style workspace browser remains a
deliberate secondary action for creating or locating work.

There is one interactive page route: `/`. API routes can use internal names and remain
independently addressable; they do not become additional product shells.

## Product language

| Product term | Meaning |
|---|---|
| Workspace | An authorized project directory that groups sessions. |
| Session | Persistent work and conversation anchored to a directory. |
| Attempt / way | One candidate solution produced in parallel with others. |
| Outcome | What changed and whether the result passed checks. |
| Route | How an attempt reasoned and acted: tools, files, commands, and trajectory. |
| Keep | Select one attempt and return its changes to the session checkout. |

Use internal words such as variant, lane, fan-out, worktree, branch, adopt, and discard in
code and APIs. On the product surface, prefer plain language. Git implementation details
must remain easy to reveal because they make the work inspectable and trustworthy.

## Runtime relationship

The one app does not replace Axocoatl's runtime strengths. It makes them legible:

- Actors provide durable agent execution and supervision.
- Memory and checkpoints let work survive beyond one request or process lifetime.
- Session isolation bounds tool execution to the chosen workspace.
- Heterogeneous providers let each attempt use a different local or remote model.
- Multi-agent session mode and the coordinator are explicit work paths; the
  event lattice carries typed notifications for Skills, triggers, webhooks, and
  retained API/WebSocket observers.
- MCP, Skills, and Automations extend what sessions and agents can do.

These are capabilities of one product. They should not compete as peer navigation
destinations.

## Definition of shipped

A feature is shipped in the app only when a person can discover it from the session,
complete it end to end, receive honest error and reconnect states, and return to the
session without changing products.

The following are necessary but not sufficient:

- a Rust type or store exists;
- a server endpoint exists;
- a custom element is defined or imported;
- a hidden legacy panel still contains the old controls;
- a unit test passes below the product seam.

Before deleting an older surface, inventory its capabilities and prove that each one is
reachable in `/`, or record an explicit product decision to remove it. Visual fusion without
runtime, state, and lifecycle fusion is not one product.

## Non-goals

- A generic consumer assistant or hosted account-first SaaS.
- A collection of framework primitives that requires users to assemble their own product.
- A second app for comparison, agents, automations, or the lattice.
- Hiding failures, git state, cost, or model choice to make the interface look simpler.
- Replacing the local-first default with a vendor-controlled runtime or data path.

Axocoatl can remain serious developer infrastructure and still have a first-class workbench.
The app is the operational face of the infrastructure, not a departure from it.
