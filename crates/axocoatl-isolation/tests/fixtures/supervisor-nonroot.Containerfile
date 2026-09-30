# Build explicitly from the already-prepared local tools image. No packages or
# network access are needed, and tests must not override this configured USER.
FROM localhost/axocoatl-supervisor-test-root:20260914
USER 1024:1024
