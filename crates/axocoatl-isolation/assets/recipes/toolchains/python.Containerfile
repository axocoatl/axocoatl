# Toolchain recipe: python. Owner: workstream toolchain.
# Appended to the python toolchain base (Python 3.13.16 on Debian 12 slim,
# git). A loadout run's setup makes a virtual environment in the
# Workspace's toolchain cache volume (mounted at /opt/axocoatl-cache), and
# every command of the Session finds it first on PATH. Bytecode, pytest's
# cache and an editable install's *.egg-info directory (setuptools reads
# DIST_EXTRA_CONFIG) are not written into the repository.
ENV AXOCOATL_TOOLCHAIN=python \
    AXOCOATL_CACHE=/opt/axocoatl-cache \
    VIRTUAL_ENV=/opt/axocoatl-cache/python/venv \
    PIP_CACHE_DIR=/opt/axocoatl-cache/python/pip \
    PIP_DISABLE_PIP_VERSION_CHECK=1 \
    PIP_NO_INPUT=1 \
    PIP_ROOT_USER_ACTION=ignore \
    PYTHONDONTWRITEBYTECODE=1 \
    PYTEST_ADDOPTS="-p no:cacheprovider" \
    DIST_EXTRA_CONFIG=/etc/axocoatl/setuptools.cfg
ENV PATH=/opt/axocoatl-cache/python/venv/bin:$PATH

RUN mkdir -p /opt/axocoatl-cache /etc/axocoatl \
 && printf '[egg_info]\negg_base = /tmp\n' > /etc/axocoatl/setuptools.cfg \
 && python3 --version \
 && python3 -m venv --help >/dev/null \
 && python3 -m pip --version >/dev/null

LABEL io.axocoatl.toolchain=python \
      io.axocoatl.toolchain.version=python-3.13.16
