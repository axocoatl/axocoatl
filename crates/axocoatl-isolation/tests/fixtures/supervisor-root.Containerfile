# Reuse already-installed repository tools without the recovery image's shell
# entrypoint. Normal Session startup must supply its usual sleep command.
FROM localhost/axocoatl-recovery-tools:alpine-3.20-v1
ENTRYPOINT []
CMD ["/bin/sh"]
