# Data root written by Axocoatl 1.4.0

`root/` is a data root that the Axocoatl 1.4.0 binary (built from the `v1.4.0` tag)
wrote, with a stand-in for Ollama 0.20.6, the only Ollama 1.4.0 accepts. It holds:

| Session | What 1.4.0 did in it |
|---|---|
| `ses-d4a64148-…` | `axocoatl run fix` (run `run-cbbb39b5-…`): one turn, its repository captures, the required check and the required review; verdict pass, signed receipt |
| `ses-ab45e515-…` | A workbench Session on `lead`: its Team applied in Team & budget, then one turn |
| `ses-6628f8ac-…` | A workbench Session never used: no Team, no turn |

Each Team holds the Ollama profile 1.4.0 recorded (schema 1, Ollama 0.20.6), which
Axocoatl 1.5 refuses until the Team is applied again.

The daemon test `bootstrap::upgrade_from_1_4_tests` (in `crates/axocoatl-daemon`)
starts the current daemon on a copy of it and checks that every Session, turn and run
is kept, and that applying the run's Team again unchanged observes its writer on the
Ollama that runs now.

The copy leaves out what each version brings or makes itself: the embedding model
(`models/`), the Linux process supervisor (`execution-supervisors/`), the receipt
signing key (`secrets/`) and the local API token. Git keeps no empty directory, and
`.gitignore` keeps out `.axocoatl-daemon.lock`, so it is stored as
`dot-axocoatl-daemon.lock` and the test copies it back under its own name, with the
data root's permissions (directories `0700`, files `0600`). The files name the paths
of the directory the fixture was made in; nothing reads them.

## Making it again

```sh
tests/fixtures/data-root-1.4.0/make/make.sh /path/to/axocoatl-1.4.0 /path/to/new-work-dir
```

`make/make.sh` needs an `axocoatl` built from `v1.4.0`, a Podman that answers,
`python3`, and ports 8373 and 8374. It starts `make/fake_ollama.py` (an Ollama 0.20.6
stand-in whose writer runs one bash command and whose reviewer approves) and the 1.4.0
daemon, makes the three Sessions above with `axocoatl run fix` and
`make/workbench_session.py`, stops the daemon and replaces `root/`. The Session and run
ids change each time; the test names them.
