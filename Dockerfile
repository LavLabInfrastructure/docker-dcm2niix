FROM ubuntu:24.04

ARG DCM2NIIX_VERSION

RUN set -eux; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl jq unzip; \
    release_json="$(curl -fsSL "https://api.github.com/repos/rordenlab/dcm2niix/releases/tags/${DCM2NIIX_VERSION}")"; \
    asset_url="$(printf '%s' "$release_json" | jq -r '.assets[] | select(.name | test("lnx.*\\.zip$")) | .browser_download_url' | head -n1)"; \
    test -n "$asset_url"; \
    curl -fsSL -o /tmp/dcm2niix.zip "$asset_url"; \
    unzip -q /tmp/dcm2niix.zip -d /tmp/dcm2niix; \
    install -m 0755 /tmp/dcm2niix/dcm2niix /usr/local/bin/dcm2niix; \
    rm -rf /tmp/dcm2niix /tmp/dcm2niix.zip; \
    apt-get purge -y --auto-remove unzip; \
    rm -rf /var/lib/apt/lists/*

ENTRYPOINT ["dcm2niix"]
