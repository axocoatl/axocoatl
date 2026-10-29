# Axocoatl 1.5.3 dashboard recording

This package contains original screenshot samples from one newly requested task in
the released Axocoatl 1.5.3 dashboard. It is an edited screen walkthrough, not a
continuous camera recording. The public provenance describes who operated the
browser, the actual model roles, the task outcome, interventions, and editing.

`integration.json` is the canonical source for clip filenames, exact durations,
and clickable chapter offsets. The full walkthrough and its review and receipt
excerpts share the same source chronology. Posters are original captured frames.
The clips contain no synthesized interface, motion interpolation, voiceover, or
fabricated result. A repeated source image fills the interval until the next
captured sample. Uncaptured intervals and any accelerated waiting intervals are
listed explicitly in `timeline.json`.

## Evidence and verification

- `provenance.json` records release identity, task configuration, outcome, media
  descriptions, and SHA-256 hashes for every media and supporting file.
- `source-*.tar.gz` hold original captured JPEG or PNG bytes plus public capture
  metadata. Archives are split to fit the site's per-file hosting limit.
- `artifact-evidence.json` preserves baseline and resulting files, changes,
  assessments, and a sanitized projection of observed host check/review state.
  Its source-response hash identifies the private raw observation; that hash alone
  is not an independent attestation of runtime behavior.
- `receipt.dsse.json` is the actual downloaded final signed receipt.
  `prior-receipt-*.dsse.json` retain earlier downloaded receipts, including failed
  or partial attempts; their signatures and corresponding artifacts are checked
  under the same signing key. Public verification
  checks its Ed25519 signature and binds its turn and file hashes to the observed
  task. `receipt-verification.json` records the CLI's offline verification against
  the final files and locally trusted signing key. The public copy removes the
  local checkout path. An embedded signing key proves integrity, not third-party
  identity or universal correctness of the change.

Run from this directory, with Node.js on PATH:

```sh
node verify-media.mjs
node verify-media.mjs --probe
```

The first command checks all file hashes, the original image dimensions,
chronology, declared edits, chapter/caption mapping, poster identity, actual task
and receipt evidence, and the signature. `--probe` additionally requires FFmpeg
and FFprobe. It decodes every frame of all three clips and compares it to a
reference reconstructed from the original samples, requiring SSIM of at least
0.99 for every frame. It also verifies dimensions, H.264/YUV420p encoding, 24 fps,
and each clip's duration. Temporary decoded reference images are removed afterward.

## Assembler inputs

`assemble-media.py` requires Python 3, Node.js, and FFmpeg. It reads finished
captures; it never drives the browser, runs an agent, invents task evidence, or
publishes files. Run `--help` for arguments. `--plan-only` validates source timing
and prints mapped clip timings without creating output. The output directory must
be new so previous attempted encodes remain available for inspection.

The finalized capture directory contains `capture.json` and a `frames/` directory.
Each frame entry names an original `frame-000000.jpg` (or `.png`) and its monotonic
`elapsed_ms`. Capture metadata records `started_at`, `finished_at`, `duration_ms`,
`theme: "dark"`, `actions`, `errors`, and explicit `{start_ms,end_ms}` recording
segments. All frames must have the same native viewport. An odd source dimension receives
one black padding row or column for H.264/YUV420p; no dashboard pixels are
cropped. The original posters remain byte-identical source captures. No original
sample may be lost through acceleration: every retained sample occupies at least
one encoded frame. An unusually short original sample is held for one encoded
frame and this small timing adjustment is explicitly listed as a `hold` edit.

The clip specification is keyed by `dashboard-flow`, `review`, and `receipt`.
Each entry has `start_ms`, `end_ms`, an original `poster` frame, and source-clock
`chapters: [{source_ms,title}]` and `captions: [{source_ms,text}]`. All three clips
are encoded from the same edited timeline. The full clip must cover every retained
sample. The assembler generates final chapter times; authors do not guess them.

An optional edit plan contains `segments` with `kind` (`speed` or `omit`),
`start_ms`, `end_ms`, a plain-language `reason`, and `rate` for accelerated waits.
Edits must align with actual sample or segment boundaries. Uncaptured gaps are
removed automatically and disclosed separately. Omissions must not hide task
failures or interventions. Private source material, if any, remains outside the
published package; its omitted frame hashes and timing still remain in the
public timeline.

The independently collected facts, final artifact evidence, downloaded DSSE
receipt, and trusted offline verification result are required inputs. Each
`--prior-receipt` argument preserves one earlier downloaded receipt. The
assembler rejects a different product version, release commit, or binary hash.
It also rejects a missing required check/review or inconsistent receipt when the
claimed task outcome is accepted.
