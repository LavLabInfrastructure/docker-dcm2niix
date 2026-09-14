FROM ubuntu:24.04

ARG DCM2NIIX_VERSION

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl unzip; \
    curl -fsSL -o /tmp/dcm2niix.zip "https://github.com/rordenlab/dcm2niix/releases/download/${DCM2NIIX_VERSION}/dcm2niix_lnx.zip"; \
    unzip -q /tmp/dcm2niix.zip -d /tmp/dcm2niix; \
    install -m 0755 /tmp/dcm2niix/dcm2niix /usr/local/bin/dcm2niix; \
    rm -rf /tmp/dcm2niix /tmp/dcm2niix.zip; \
    apt-get purge -y --auto-remove unzip; \
    rm -rf /var/lib/apt/lists/*

ENTRYPOINT ["dcm2niix"]
