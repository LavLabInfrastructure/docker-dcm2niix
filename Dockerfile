FROM ubuntu:24.04

ARG DCM2NIIX_VERSION
ARG GITHUB_TOKEN

RUN set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl jq unzip; \
    if [ -n "${GITHUB_TOKEN:-}" ]; then \
      release_json="$(curl -fsSL -u "${GITHUB_TOKEN}:x-oauth-basic" "https://api.github.com/repos/rordenlab/dcm2niix/releases/tags/${DCM2NIIX_VERSION}")"; \
    else \
      release_json="$(curl -fsSL "https://api.github.com/repos/rordenlab/dcm2niix/releases/tags/${DCM2NIIX_VERSION}")"; \
    fi; \
    asset_url="$(printf '%s' "$release_json" | jq -r '.assets[] | select(.name | test("lnx.*\\.zip$")) | .browser_download_url' | head -n1)"; \
    test -n "$asset_url"; \
    test "$asset_url" != "null"; \
    curl -fsSL -o /tmp/dcm2niix.zip "$asset_url"; \
    unzip -q /tmp/dcm2niix.zip -d /tmp/dcm2niix; \
    install -m 0755 /tmp/dcm2niix/dcm2niix /usr/local/bin/dcm2niix; \
    rm -rf /tmp/dcm2niix /tmp/dcm2niix.zip; \
    apt-get purge -y --auto-remove unzip; \
    rm -rf /var/lib/apt/lists/*

ENTRYPOINT ["dcm2niix"]
