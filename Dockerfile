# syntax=docker/dockerfile:1.7
FROM ubuntu:24.04

ARG DCM2NIIX_VERSION
ARG TARGETARCH

RUN --mount=type=secret,id=github_token \
    set -eu; \
    apt-get update; \
    apt-get install -y --no-install-recommends ca-certificates curl jq unzip; \
    if [ -s /run/secrets/github_token ]; then \
      github_token="$(cat /run/secrets/github_token)"; \
      release_json="$(curl -fsSL -u "${github_token}:x-oauth-basic" "https://api.github.com/repos/rordenlab/dcm2niix/releases/tags/${DCM2NIIX_VERSION}")"; \
    else \
      release_json="$(curl -fsSL "https://api.github.com/repos/rordenlab/dcm2niix/releases/tags/${DCM2NIIX_VERSION}")"; \
    fi; \
    if [ "${TARGETARCH:-amd64}" != "amd64" ]; then \
      echo "Unsupported TARGETARCH: ${TARGETARCH:-unknown}" >&2; \
      exit 1; \
    fi; \
    asset_url="$(printf '%s' "$release_json" | jq -r '.assets[] | select((.content_type == "application/zip") and (.name | test("(lnx|linux)"; "i")) and (.name | test("arm|aarch64"; "i") | not)) | .browser_download_url' | head -n1)"; \
    test -n "$asset_url"; \
    test "$asset_url" != "null"; \
    curl -fsSL -o /tmp/dcm2niix.zip "$asset_url"; \
    unzip -q /tmp/dcm2niix.zip -d /tmp/dcm2niix; \
    binary_path="$(find /tmp/dcm2niix -type f -name dcm2niix | head -n1)"; \
    test -n "$binary_path"; \
    install -m 0755 "$binary_path" /usr/local/bin/dcm2niix; \
    rm -rf /tmp/dcm2niix /tmp/dcm2niix.zip; \
    apt-get purge -y --auto-remove unzip; \
    rm -rf /var/lib/apt/lists/*

ENTRYPOINT ["dcm2niix"]
