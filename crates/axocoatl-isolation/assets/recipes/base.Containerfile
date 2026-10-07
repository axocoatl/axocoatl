# Base of every recipe image. Owner: workstream agents.
# Not implemented: the agents workstream pins the base image by digest (Node
# 24 >= 24.8 on Debian bookworm slim, so e2e@0.18.0's Node floor holds) and
# installs the POSIX/Git command surface Axocoatl probes for.
