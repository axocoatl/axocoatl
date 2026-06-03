# Axocoatl brand & voice

Single source of truth for how Axocoatl writes, looks, and feels. If a
page, doc, or post can't be reconciled with this file, the page is
wrong, not the file. The goal is consistency that compounds: every page
reinforces every other page, and a year from now we still sound like
ourselves.

Last updated: 2026-06-01.

---

## 1 · What it's actually for

Axocoatl exists for one reason: **the agent-tooling industry has a
theater problem, and we're for the teams who actually ship.**

The competition optimizes for the *aesthetic* of AI work — Mac mini
farms, glowing terminals, demo videos, screenshots of conversations.
Their videos are great. Their throughput is theoretical. Their users
churn after the demo high wears off.

We optimize for the *unglamorous reality* — agents that run reliably
for months, persist their state through restarts, follow real
workflows, and finish the work. The release notes that ship every
Monday. The support triage that runs at 3 a.m. The contract review
that lands in legal's inbox at 9 sharp. Real workflows. Not demos.

If a piece of copy could appear on a competitor's site without changing
its meaning, the copy is wrong.

## 2 · Positioning statement

> Axocoatl is the open-source agentic runtime for teams that ship.
> Real workflows. Local-first. Your hardware, your LLM, your data.
> One 25 MB Rust binary that runs, supervises, and survives.

Three pillars. Every page should reinforce one or more:

1. **Built for production.** Actor-supervised, checkpointed, sandboxed.
   Survives restarts. Runs as a system service.
2. **Yours by default.** Your hardware. Your LLM. Your data. Air-gappable.
3. **Real workflows, not chat.** The lattice cascades work between
   agents automatically. Schedules, proactive triggers, automations.

## 3 · Voice

We sound like a senior engineer giving a confident, precise briefing to
peers. Not a salesperson. Not a marketer. Not a futurist. Not a bro.

**Adjectives that fit:** precise, dry, confident, occasionally wry,
specific, declarative.

**Adjectives that don't:** breathless, salesy, breathless again, jargon-
heavy, futuristic, mystical, urgent, hyperbolic.

### Good

> "Axocoatl is a runtime, not a framework. The agents run, supervise
> themselves, and persist their state. The lattice does the routing."

> "The agent-tooling industry has a theater problem."

> "Real workflows. Not demos."

> "One 25 MB binary. Your hardware. Your LLM. Your data."

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
2. **Declaration** (a confident statement of fact): *The lattice does
   the routing.*
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
| The runtime | the daemon | — | Lowercase. It's the running process. |
| Coordination fabric | the event lattice / Studio | — | "Lattice" capitalized only as the product name (Studio). |
| Unit of work | Workflow, Automation | a workflow, an automation | |
| LLM-backed actor | Agent | an agent | |
| Lattice-aware capability | Skill | a skill | (Not "skill" as in "skills.")|
| Directory work surface | Session | a session | |
| Activation tier | Lattice mode, Single-agent mode | — | These are SessionMode variants. |
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
  hyperlinks in body, the lattice-edge highlight, code-block keywords.
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

- **The lattice canvas** pulses on a scripted loop (12–15 seconds) to
  show the product is alive.
- **Hover lifts** on cards: 1px translateY, 120ms ease-out, border shifts
  from `--border` to `--accent`.
- **Theme morph** when the toggle fires: 200ms cross-fade on every
  background and text color. Nothing else animates with the theme.

Forbidden motion: parallax, scroll-jacking, decorative blob shaders,
hero videos that autoplay with sound, lottie animations, anything that
moves while the user is reading.

### Iconography

The Axocoatl mark is the only logo. No icon set.

Inline glyphs in copy use the existing typographic set the dashboard
already established:

- `◉` watch / observe
- `◇` skill / event
- `⌬` lattice / cluster
- `▣` session / contained workspace
- `⟳` automation / cycle
- `◫` docs / pages
- `→` go / next
- `·` separator between meta items

Don't introduce new glyphs. If you need a new one, add it to this list
first.

## 9 · The lattice demo as a brand asset

The hero on every relevant page should embed the live `<ax-lattice>`
component running a scripted loop. This is our equivalent of Stripe's
animated illustrations: we sell the product by showing the product.

The scripted loop on the homepage is the canonical demo: a two-agent
research→summarize workflow that pulses through the lattice on a
12-second cycle. The same scripted loop runs in three places:

- Homepage hero
- `/concepts` lattice section
- `/install` page footer

Don't write a *different* scripted loop for each page. The repetition
is the point — the lattice is the brand asset.

## 10 · Comparison frame

When we compare against the competition, we *never* name names. The
reader knows. Naming names invites a flame war we don't need; not
naming them lets the reader project whichever competitor they were
already frustrated with.

Acceptable framings:

- "Most agent frameworks." (referring to the open-source Python
  framework crowd)
- "Personal coding copilots." (referring to single-developer IDE-bound
  agent tools)
- "Hosted agent platforms." (referring to closed cloud assistant /
  workbench products)

Unacceptable: naming any specific competitor anywhere on the site.

## 11 · Social proof rules

We don't fabricate. Until we have real testimonials, real customer
logos, real download numbers, real GitHub stars — we don't put them on
the site. The slot stays empty, or we use a *factual* trust signal:

- "v0.1 · 340+ tests · Apache 2.0"
- "25 MB Rust binary · zero telemetry"
- "Runs against Ollama, OpenAI, Anthropic, Mistral, Gemini"
- "MIT/Apache dual license"

Once we have real adoption signals, we lead with them. The first real
GitHub-stars number that's worth showing is 500+. The first real
testimonial gets a name and a face. We don't use stock photos or
synthetic names. If a testimonial is real but the person wants to be
anonymous, we still cite the company and the role.

## 12 · OpenRouter positioning

OpenRouter is a *first-class* recommended provider in Axocoatl, alongside
Ollama (local) and the direct provider clients (OpenAI / Anthropic /
Mistral / Gemini). The `axocoatl onboard` wizard surfaces it as the
"easiest cloud option."

The marketing site has a dedicated `/integrations/openrouter` page that:

- Shows the one-line config to swap Ollama for OpenRouter.
- Shows how to pick a model from OpenRouter's catalog.
- Links to OpenRouter's app directory so users can see Axocoatl listed.

OpenRouter is the only third-party LLM router we feature this prominently
on the marketing site. Other routers (Helicone, Portkey, LiteLLM) get a
mention in the docs but not a dedicated page.

## 13 · The blog and changelog

When we write blog posts, they follow the same voice. Title patterns:

- **Engineering** (most common): "How Axocoatl handles X." — declarative,
  technical, specific. Example: "How Axocoatl persists agent state
  across restarts."
- **Concept** (occasional): "On X." — short, essayistic, no clickbait.
  Example: "On stigmergy." or "On theater in AI tooling."
- **Release notes** (every minor + major): "v0.X.0 — release notes." —
  no marketing prose, just what changed and why.

The changelog is *not* the same as release notes. The changelog lives at
`docs.axocoatl.ai/changelog` and is the bullet-by-bullet record. The
release notes blog post is the curated story.

## 14 · What stays the same forever

Some things we won't change with the wind, because they're load-bearing.

- The positioning. We are for teams that ship; we are against AI
  theater. This doesn't change because the optics industry doesn't
  change.
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
