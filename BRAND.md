# Axocoatl brand and voice

Source of truth for how Axocoatl writes, looks, and feels. Product structure and
terminology live in `docs/PRODUCT.md`; implementation determines what ships. If a
page, doc, or post conflicts with current product fact, fix the fact source and this
file together. The goal is consistency that compounds: every page reinforces every
other page, and a year from now we still sound like ourselves.

Last updated: 2026-09-30.

---

## 1 · What it's actually for

Axocoatl exists for one reason: **agent tooling has a theater problem, and
engineers need the work to hold up in a repository.**

Polished demos and final answers can hide the facts that matter: who changed what,
what executed, what failed, what it cost, and what remains for the engineer to decide.

We optimize for the unglamorous reality: agents that work against real files on
models the engineer chooses, including small local ones; one Agent that writes
while helpers only read; checks and review that the host runs instead of taking
the model's word; limits a person approves; and a record of every activation,
tool call and budget decision. The repository stays under the engineer's control,
and Git shows what changed. When a problem has several plausible solutions, the
same workbench can compare real attempts without turning them into unrelated chats.
Real workflows. Not demos.

If a piece of copy does not name a fact the code and reachable product can prove,
the copy is wrong.

## 2 · Positioning statement

> Axocoatl is an open-source, local-first harness for coding agents. A lead Agent
> writes; read-only helpers answer its questions and review its work; the host can
> require checks and review before a turn completes. Every activation, tool call
> and budget decision is recorded in one durable Session, with the repository,
> terminal, preview and Git around it.

Three pillars. Every page should reinforce one or more:

1. **A lead and read-only helpers.** The lead owns the change. Helpers start in an
   empty conversation, answer one task, and cannot change files. Several can run at
   once; the benefit is a clean lead context, not speed.
2. **The host decides, and records.** Required checks, required review, write scopes,
   grants and budgets are applied by Axocoatl, not requested of the model. Every
   activation, tool call and budget decision is retained in the Session. Several
   Ways are an optional decision mode inside this surface, not the product category.
3. **Yours and local-first.** One executable on your machine, local models through
   Ollama, hosted models through OpenRouter. Bring Agents, Skills, MCP servers, and
   Automations. No Axocoatl telemetry; integration and network paths are explicit.

### Claims we do not make

- **No result claims for the default team.** Do not say the lead-and-helpers team
  produces better results, or beats a single Agent, until a comparative evaluation
  is published. Then state its result and its scope, nothing wider.
- **No speed claims for helpers.** Helpers share one checkout, so their tool
  processes queue, and a local model server limits how many model calls run at once.
- **Isolation is not the headline.** Say what runs where: "Tools run in a rootless
  Podman container. Network access is on by default; set `network: none` for
  repositories you don't trust." Do not call Axocoatl or its sandbox "secure",
  "safe" or "hardened", and do not claim egress control, credential isolation or
  zero trust.
- **Write scopes, stated narrowly.** Read-only helpers get no file-writing tools, and
  their shell runs under Landlock where Linux 6.2 or later allows it; otherwise they
  get no shell. A path-scoped writer's file tools refuse other paths, and every change
  it made is checked after it finishes. For a writer with a shell, that check is
  review evidence, not confinement.
- **Retired vocabulary.** The founding stigmergy thesis was built, measured and
  removed in 1.1.0. Do not describe Axocoatl with "stigmergy", "pheromones", "signal
  field", "swarm", "without a manager" or "no central orchestrator". Do not call it
  "the go-to" anything. The marketing and docs validators reject these terms outside
  the changelog.

## 3 · Voice

We sound like a senior engineer giving a confident, precise briefing to
peers. Not a salesperson. Not a marketer. Not a futurist. Not a bro.

**Adjectives that fit:** precise, dry, confident, occasionally wry,
specific, declarative.

**Adjectives that don't:** breathless, salesy, breathless again, jargon-
heavy, futuristic, mystical, urgent, hyperbolic.

### Good

> "Axocoatl is a workbench backed by a runtime, not a framework you have to turn
> into a product. The agents run, persist, and show their work in one session."

> "The agent-tooling industry has a theater problem."

> "Real workflows. Not demos."

> "One Rust binary. Your repository. Your model choices. Your decision."

> "Close the laptop. Open it tomorrow. The session is still there."

### Bad

> "Unleash the power of AI agents with our revolutionary platform!" — sales
> "🚀 Supercharge your team with autonomous AI workflows!" — bro
> "Axocoatl reimagines what's possible with multi-agent systems." — vacuous
> "AI-powered. Cloud-native. Lightning-fast." — bingo card
> "We believe AI should serve humanity, not replace it." — pious

### Forbidden words

The following words and phrases never appear in shipped Axocoatl copy:

- **unleash**, **supercharge**, **revolutionize**, **transform**, **reimagine**
- **delight** as a verb
- **leverage** as a verb
- **lightning-fast**, **blazing-fast**, **next-generation**, **next-gen**
- **AI-powered**, **AI-native** (use "agentic" if you need a category word)
- **seamless**, **frictionless** (almost always meaningless)
- **journey** in a metaphorical sense
- **users love** anything (we say what they do, not how they feel)
- emoji in copy — em-dashes do the work em-dashes do
- exclamation marks (except in code blocks)

## 4 · Tone calibration

Tone is voice plus situation. The same voice goes through three settings:

| Setting | When | How it sounds |
|---|---|---|
| **Plain** | Most copy — hero, concepts, docs body | Direct sentences. One claim per clause. Concrete nouns. |
| **Wry** | Headlines, taglines, captions on demos | A single dry observation. No setup, no payoff hunting. |
| **Technical** | Reference docs, the inside of feature pages | Precise nouns. No marketing prose. Code first, prose second. |

The dial is *never* "excited." If you find yourself writing copy that
reads with rising enthusiasm, cut it.

## 5 · Headline patterns

We use four headline shapes. Pick the one that fits the page; don't mix.

1. **Verb–noun** (the work the user does): *Plan and ship work that
   agents actually do.*
2. **Declaration** (a confident statement of fact): *Explicit work. Shared
   events.*
3. **Contrast** (us vs. the genre): *Real workflows. No theater.*
4. **Imperative** (a command, used sparingly): *Stop performing AI.
   Start running it.*

Headlines are short. Three to seven words. The subheadline does the
explaining. Capitalize like a sentence, never like a title.

## 6 · CTA patterns

Buttons get short, direct verbs. Never "Click here," "Learn more,"
"Discover," or "Find out how."

| Use case | Primary | Secondary |
|---|---|---|
| Top of page | "Install" or "Get started" | "Read the docs" |
| End of section | "How it works →" | "See the showcase" |
| Pricing-like | "Use it for free" (technically true — open source) | "View the source" |
| Docs cross-link | "Concepts →" | (none) |

The em-dash arrow `→` is the only ornament we use on CTAs. No icons in
buttons unless they're literally part of the meaning (a copy icon, an
external-link icon).

## 7 · Naming conventions

These are the names of the things. They're capitalized when they refer
to the system concept, lowercase when they refer to instances of it.

| Concept | Capital | Lowercase | Notes |
|---|---|---|---|
| The product | Axocoatl | — | Always one word, capitalized A. Never "axocoatl" in body copy. |
| The runtime | the daemon | — | Lowercase. It is the running process behind the app. |
| Event transport | the event feed | — | Carries the events Skills publish. An engine capability, not a destination. |
| Unit of work | Workflow, Automation | a workflow, an automation | |
| LLM-backed actor | Agent | an agent | |
| Event-publishing capability | Skill | a skill | (Not "skill" as in "skills.")|
| Authorized project directory | Workspace | a workspace | Groups sessions. |
| Persistent work item | Session | a session | Anchored to a workspace; chat is its spine. |
| Parallel candidate | Attempt | an attempt / a way | Do not expose lane or variant as the primary noun. |
| Team roles | Lead, helper | a lead, a helper | The lead writes; helpers are read-only. Scout and Reviewer are the default helpers. |
| Completion conditions | required checks, required review | — | Opt-in. The host runs them, not the lead. |
| Approved authority | grant, budget | a grant | The limits a person applies in Team & budget. |
| Session team choice | Single agent, Lattice team, Custom team | — | Labels in the New session picker. A Lattice team is a team defined in configuration; do not use "lattice" for anything else in copy. |
| Marks | Mark, wordmark | — | Lowercase in copy unless start of sentence. |
| Marketplace integration | MCP server | — | MCP all caps; "server" lowercase. |

**Never abbreviate Axocoatl.** No "Axo," no "AX," no "Coatl." If space is
tight, use the mark alone (no text).

## 8 · Visual system

Tokens are in `branding/colors.json` and the synced `tokens.css` files in
each site. The system has *one* primary, *one* secondary, *one* accent.
That's it.

### Color

- **Jade** (`#3E7C5C`) is primary. The serpent. Buttons, links, focus
  rings, the only "brand" color the user touches.
- **Bronze** (`#B5904A`) is secondary. Use sparingly — section
  dividers, the occasional warm flourish.
- **Blue** (`#3FA9C8`) is the accent for "tech inside myth" moments —
  hyperlinks in body, graph-edge highlights, code-block keywords.
- **Neutrals** are 90% of any page. Ink for dark mode, parchment for
  light mode, white/black at the ends. Never use pure black or pure
  white as a page background.

Don't introduce new colors. If you need to distinguish two things, use
weight or position, not color.

### Typography

- **Display: Space Grotesk**, weights 500/600. Headings, hero, CTAs,
  navigation. Letter-spacing −.012em for tight optical kerning.
- **Body: system-ui**. The user's native font. Never override the body
  to a webfont — system-ui is fast, familiar, and reduces FOUT to zero.
- **Mono: JetBrains Mono**. Code, terminal blocks, file paths,
  command-line snippets. Weights 400/500 only.

Modular scale, 1.25 ratio:
```
13px  small / captions
14px  body small / labels
15.5px body default
20px  h4
24px  h3
32px  h2
44px  h1 (page titles)
64px  hero (display)
```

### Spacing

Spacing follows a 4-pixel base. Always pick from the scale: `4, 8, 12,
16, 24, 32, 48, 64, 96, 128`. Never use a custom value.

Section padding (`<section>` → next `<section>`) is `96px` on desktop,
`64px` on mobile. Container max-width is `1180px` for general content,
`760px` for prose.

### Layout

Three layouts; pick one per section:

1. **Full-bleed centered hero**: title + sub + CTAs + a single demo.
2. **Two-column (1.3 / 1)**: prose on the left, demo or diagram on the
   right. Used by every "concept" section.
3. **Three-column auto-fit (≥260px)**: feature/pillar grid. Used at most
   twice per page.

No carousels. No sliders. No accordions. No tabs above the fold.

### Motion

Motion is restrained. The whole site has three motion patterns:

- **Product films** are muted recordings of the product. Only a hero film may
  autoplay, and never when the visitor prefers reduced motion. Caption each film
  with what the recording actually shows.
- **Hover lifts** on cards: 1px translateY, 120ms ease-out, border shifts
  from `--border` to `--accent`.
- **Theme morph** when the toggle fires: 200ms cross-fade on every
  background and text color. Nothing else animates with the theme.

Forbidden motion: parallax, scroll-jacking, decorative blob shaders,
hero videos that autoplay with sound, lottie animations, anything that
moves while the user is reading.

### Iconography

The Axocoatl mark is the only logo. No icon set.

Inline glyphs in copy use the existing typographic set the app
already established:

- `◉` watch / observe
- `◇` skill / event
- `⌬` team / graph
- `▣` session / contained workspace
- `⟳` automation / cycle
- `◫` docs / pages
- `→` go / next
- `·` separator between meta items

Don't introduce new glyphs. If you need a new one, add it to this list
first.

## 9 · Product demonstrations

Show the actual one-app Session whenever a page is about the product: durable
conversation, repository context, visible agent execution, focused workspace tools,
and deliberate Git review. The default demonstration should show one agent completing
normal repository work. A Ways-specific page may extend that Session into several
attempts, visible Checks and comparison, one kept result, and Git review. Do not make
parallelism the premise of a general product demonstration.

A demonstration must match a reachable current workflow. Never label a mock 1:1 or
exact unless it was compared to the current app in the same change.

A homepage hero must show the current workbench rather than lead with Ways, the
Agent graph, or another subsystem. Diagrams in code panes may explain the lead,
helpers, checks and review, but they do not substitute for showing the workbench.

## 10 · Comparison frame

When we compare against the competition, we *never* name names. The
reader knows. Naming names invites a flame war we don't need; not
naming them lets the reader project whichever competitor they were
already frustrated with.

Acceptable framings:

- "Most agent frameworks." (referring to the open-source Python
  framework crowd)
- "Opaque single-answer coding agents." (referring to tools that hide execution and
  comparison)
- "Hosted agent platforms." (referring to closed cloud assistant /
  workbench products)

Unacceptable: naming any specific competitor anywhere on the site.

## 11 · Social proof rules

We don't fabricate. Until we have real testimonials, real customer
logos, real download numbers, real GitHub stars — we don't put them on
the site. The slot stays empty, or we use a *factual* trust signal:

- "Apache-2.0 licensed"
- "One Rust binary · no Axocoatl telemetry"
- "Provider adapters for Ollama, OpenAI, OpenRouter, Anthropic, Mistral, Gemini"

Once we have real adoption signals, we lead with them. The first real
GitHub-stars number that's worth showing is 500+. The first real
testimonial gets a name and a face. We don't use stock photos or
synthetic names. If a testimonial is real but the person wants to be
anonymous, we still cite the company and the role.

## 12 · OpenRouter positioning

OpenRouter is a supported provider in Axocoatl, alongside
Ollama (local) and the direct provider clients (OpenAI / Anthropic /
Mistral / Gemini). The `axocoatl onboard` wizard offers it as one cloud option.

The marketing site has a dedicated `/integrations/openrouter` page that:

- Shows the config to select OpenRouter per agent.
- Tells users to choose a model id available to their OpenRouter account.
- Explains the attribution headers Axocoatl sends without claiming a current
  directory listing or traffic rank.

OpenRouter is the third-party LLM router with a dedicated marketing page.

## 13 · The blog and changelog

When we write blog posts, they follow the same voice. Title patterns:

- **Engineering** (most common): "How Axocoatl handles X." — declarative,
  technical, specific. Example: "How Axocoatl persists agent state
  across restarts."
- **Concept** (occasional): "On X." — short, essayistic, no clickbait.
  Example: "On theater in AI tooling."
- **Release notes** (every minor + major): "v0.X.0 — release notes." —
  no marketing prose, just what changed and why.

The changelog is *not* the same as release notes. The changelog lives at
`docs.axocoatl.ai/changelog` and is the bullet-by-bullet record. The
release notes blog post is the curated story.

## 14 · What stays the same forever

Some things we won't change with the wind, because they're load-bearing.

- The product is one workbench backed by one runtime. Runtime subsystems do not become
  separate products because a feature is easier to demo that way.
- We are for engineers who ship and against AI theater. The work must be inspectable.
- The mark. Single mark. No mascot. No alternate logo for "playful"
  contexts. The serpent is the brand.
- Apache-2.0. We don't relicense. We don't dual-license. We don't move
  to SSPL.
- Local-first. The runtime always runs on the user's hardware. We don't
  ship a hosted version that becomes the primary offering.
- Rust. The runtime is Rust. We don't rewrite in Go or Python because
  trends.

These commitments are part of the brand. Breaking any of them requires
a brand revision, not a feature decision.

## 15 · Who maintains this file

The brand is owned by the project. Significant edits go through PR
review like code. Small edits — typos, clarifications — go straight to
main. If something on a marketing page or in the docs feels off, edit
this file first to make the rule explicit, then change the page to
match.

The implicit deal: every contributor agrees to the rules in this file
when they write copy or design a page. New rules go through the same
review process as new code. The file is short on purpose — every rule
in it is here because we've already seen it broken.
