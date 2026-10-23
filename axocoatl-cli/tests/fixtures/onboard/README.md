# Configurations earlier releases' onboarding wrote

Each file is the text an earlier `axocoatl onboard` wrote, rendered from that
release's own template (`git show TAG:axocoatl-cli/src/main.rs`), byte for byte:

- `config-1.4.0-ollama.yaml`: v1.4.0 `local_onboarding_configuration` with the
  base URL `http://localhost:11434`, Lead `qwen3-coder:30b` and Reviewer
  `gpt-oss:20b`. 1.2.0 to 1.3.2 wrote the same text; 1.1.x wrote it with one
  model for every Agent.
- `config-1.4.0-openrouter.yaml`: v1.4.0 `hosted_onboarding_configuration` for
  OpenRouter with `qwen/qwen3-coder` and no key typed (`${OPENROUTER_API_KEY}`).
- `config-1.0.0-ollama.yaml`: v1.0.0's local Ollama setup with `llama3.2`, which
  onboarding then wrote into a new project directory.

`onboard --update` reads them as onboarding's own (the Agents of 1.1.0 and
later move to the models chosen) or keeps them (1.0.0's Assistant, which has no
tools).
