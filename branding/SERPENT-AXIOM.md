# Serpent character axiom

Source of truth for the Axocoatl serpent character.  Every generation
that includes the serpent — mark, wordmark, lockups, marketing
illustration, animated transitions — must satisfy this description AND
pass `branding/mark.png` as the visual reference.

## Identity

An Aztec / Nahua **water-serpent dragon**, named for the
*axōcōātl* (the water-serpent god).  One distinct creature, always
recognisable across poses.

## Visual properties (non-negotiable)

| Trait      | Specification                                                                                  |
|------------|------------------------------------------------------------------------------------------------|
| Body       | Rich **jade green** scales (`#3E7C5C` family), each scale individually rendered with a soft inner highlight.  Body cross-section round, supple, organic. |
| Head       | Polished **bronze / gold** plating (`#B5904A` family) — stylised Aztec dragon-mask with carved scrollwork brow, prominent jaw, sweeping mane / horn fronds rendered as individual feather quills. |
| Eye        | A single bright **cyan / electric blue** eye (`#3FA9C8` glow), inset like a gem.               |
| Inner chasing | Bronze decorative ornament lining the inner edge of the body — scroll-flourishes, fine lattice, Mesoamerican step-fret / meander-key bands — same metallic finish as the head. |
| Cloud-curls | Bronze scroll / cloud-curl ornaments inside the body coil (from the source sketch). |
| Lighting   | Soft warm rim light from upper-right; specular highlights on scales + bronze.                  |
| Line work  | Premium illustrative finish — not flat, not photoreal; closer to high-end studio illustration / heraldic plate. |

## Pose (varies by use)

- **Mark** (`mark.png`): full **ouroboros**, head bites tail, perfect circle.
  The canonical pose, derived from `branding/sketch-reference.png` and
  rendered via `tools/imagegen.py` (gemini-3-pro-image-preview, three-
  reference workflow).
- **Wordmark** (`branding/wordmark.png`): the mark inlined between the
  letters `ax` and `atl` set in Space Grotesk SemiBold; the ouroboros
  IS the `oco` of *axocoatl*.
- Future poses should always state the pose explicitly while preserving
  every trait in the table above.

## Reference image

`branding/mark.png` is the visual reference for any new serpent
generation.  Pass it as `--ref` to `tools/imagegen.py`.

## What this is NOT

- Not a Chinese / European dragon — wing-less, four-legless, no claws.
- Not photoreal — illustrated style only.
- Not chibi / cute — serious, regal, ancient.
- Not psychedelic / glitch — the metallic + organic finish is consistent
  studio illustration.
- Not stone-carved relief — a polished living character with depth, not
  an etched ornament.
