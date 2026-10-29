# 1.5.2 acceptance decision

## Maintainer instruction

For the dashboard receipt correction, the maintainer explicitly selected actual
browser use as acceptance:

> THe only acceptance I want is you actually using the dashboard in the browser as a user would use it, and being 100% sure that its ready.

This decision applies to 1.5.2. It is not a carry-forward of the 1.5.1 exception.
The prior instruction to ship the patched release and deploy both sites remains
in effect. The publication workflow itself is unchanged: its automated gates
still have to pass, and publication must use its explicit, logged founder
new-user-evidence exception for the fresh human gate paths not rerun here.

## Exercised product behavior

The dashboard receipt change is commit
`847ce5841ed03159fcf5884c8410b2684147a9be`. The browser exercise used the rebuilt
embedded application and real local model execution. The flow opened a workspace,
applied a team, submitted a request, observed the lead, required checks and
required review, inspected the resulting files and downloaded the signed receipt.
The successful team used GPT-OSS 20B for the lead and a separate reviewer
activation. A Qwen 30B reviewer failure was retained as needing attention rather
than reported as a pass. A cancelled turn retained its interrupted outcome and
checks and review that did not run.

After restarting the daemon and submitting a later request, downloading the earlier
finished turn's receipt from History returned byte-identical signed bytes. Offline
verification against the trusted signing key and captured files passed. The
correction excludes later runtime repository reattachments from the receipt cache
fingerprint while preserving the original execution capture evidence.

These observations establish the affected dashboard journey. They are not a
fresh installation or provider sign-in certification, a comparative model result,
or a claim that a signed receipt proves code correctness.

## Release-gate scope

The following full fresh-human release-gate paths are not rerun for 1.5.2:
`C1-local`, `C1-claude-code`, `C1-codex`, `C1-openrouter`, `C1-service`,
`C1-workbench-webkit`, `C1-recovery`, `C1-real-repo`, and live-provider Linux `D1`.
The focused browser exercise above does not relabel any of those complete paths
as passed. Earlier evidence remains attached to its original version and source.

The release notes disclose this scope. A release is published only after the
unchanged automated publication workflow succeeds; preparation of this document
does not claim publication or workflow completion.
