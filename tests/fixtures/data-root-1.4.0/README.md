# Data root written by Axocoatl 1.4.0

`root/` is a data root that the released Axocoatl 1.4.0 binary wrote (from
`axocoatl-v1.4.0-aarch64-apple-darwin.tar.gz`, SHA-256
`1291c5cc7227a50b75f7db874fae3a3d34f8b8cd28f2ea9ca4ac3afc028c55bc`), with a stand-in for
Ollama 0.20.6, the only Ollama 1.4.0 accepts. It holds:

| Session | What 1.4.0 did in it |
|---|---|
| `ses-d14ac217-…` | `axocoatl run fix` (run `run-5613f837-…`): one turn, its repository captures, the required check and the required review; verdict pass, signed receipt |
| `ses-774ef092-…` | A workbench Session on `lead`: its Team applied in Team & budget, then one turn |
| `ses-4efe1aa0-…` | A workbench Session on `lead` whose Team lets it delegate to the read-only helpers `reviewer` and `scout`, then one turn in which `lead` delegated to `scout` |
| `ses-c466c04c-…` | A workbench Session never used: no Team, no turn |

Each Team, and each helper it approved, holds the Ollama profile 1.4.0 recorded
(schema 1, Ollama 0.20.6, at `http://127.0.0.1:8373`), which Axocoatl 1.5 refuses until
the Team is applied again. Every budget has expired: the run's an hour after it started
(its loadout's `wall_clock`), the others 24 hours after their Team was applied.

The daemon test `bootstrap::upgrade_from_1_4_tests` (in `crates/axocoatl-daemon`)
starts the current daemon on a copy of it and checks that every Session, turn and run
is kept. Before the Team is applied again, each Agent and helper refuses with the
0.20.6 message when connected at the recorded endpoint, as after an upgrade in place
(the test runs no Ollama there, so the message is usually the one for an Ollama that
does not answer), and the Team exactly as it is is refused because its budget expiry
has passed. Applied again the way Team & budget's Edit drafts it (a new expiry, nothing
else changed), its Agents and helpers are observed again on the Ollama that runs now,
with the 0.20.6 reason in Preview, and each connects; applied once more, they are kept.
The test's Ollama runs at another endpoint than the recorded one, so the profiles are
refused for that as well; the 0.20.6 reason comes first.

The copy leaves out what each version brings or makes itself: the embedding model
(`models/`), the Linux process supervisor (`execution-supervisors/`), the receipt
signing key (`secrets/`) and the local API token. Git keeps no empty directory, and
`.gitignore` keeps out `.axocoatl-daemon.lock`, so it is stored as
`dot-axocoatl-daemon.lock` and the test copies it back under its own name, with the
data root's permissions (directories `0700`, files `0600`). The files name the paths
of the directory the fixture was made in, `/private/tmp/axo140`. The test makes the
repository `make/make.sh` starts from in its own temporary directory and points the
copy's Workspace and Sessions (`workspaces/`, `sessions/`) at it, so the daemon finds
each Session's Workspace folder, as after an upgrade in place. What the History
recorded about past work (where it ran, the run's manifest) keeps the path it was done
at.

## Making it again

```sh
tests/fixtures/data-root-1.4.0/make/make.sh /path/to/axocoatl-1.4.0 /tmp/axo140
```

The fixture is public, so the work directory has a neutral path that names no person
(on macOS it must be under `/private` or the home folder, which the Podman machine
shares). `make/make.sh` needs Axocoatl 1.4.0 (the released binary or one built from the
`v1.4.0` tag), a Podman that answers, `python3`, and ports 8373 and 8374. It starts
`make/fake_ollama.py` (an Ollama 0.20.6 stand-in whose writer delegates to its helper or
runs one bash command, whose helper answers and whose reviewer approves) and the 1.4.0
daemon, makes the four Sessions above with `axocoatl run fix` and
`make/workbench_session.py`, stops the daemon and replaces `root/`. The Session and run
ids change each time; the test names them.
