# Lobsters

## Title

Axocoatl: an agentic runtime in Rust, with supervised actors and a stigmergic event lattice

## URL

https://axocoatl.ai

## Category

`show` and `rust`

## Comment

Author here. Axocoatl is a multi-agent runtime that takes a different
shape than the usual framework — agents are supervised `ractor` actors
with checkpointed state, and coordination happens through a stigmergic
event lattice rather than a central scheduler. Local-first, runs as a
system service, one 25 MB binary.

The lattice piece is the bit I'd most welcome critique on. Agents
declare `depends_on` and publish `TaskCompleted` events; downstream
agents activate when their threshold is met. It feels right for the
unglamorous recurring workflows I actually wanted agents to do (release
pulse, support triage, daily briefings) but I'd be curious whether the
stigmergic model breaks down at scales I haven't hit yet.

Repo: https://github.com/axocoatl/axocoatl
Concepts page with a live lattice demo: https://axocoatl.ai/concepts
