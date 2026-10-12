# The same runtime test image with a configured non-root USER. ROOT_IMAGE is
# the root image scripts/test-exec-supervisor-runtime.sh built; nothing is
# installed or downloaded, and the tests never override this USER.
ARG ROOT_IMAGE
FROM ${ROOT_IMAGE}
USER 1024:1024
