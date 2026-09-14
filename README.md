# docker-dcm2niix

latest dcm2niix in a simple image

## Automated monthly updates

A scheduled GitHub Actions workflow (`.github/workflows/monthly-dcm2niix-update.yml`) checks
the latest `rordenlab/dcm2niix` release every month.

If an image tag for that release does not already exist in GHCR, the workflow builds and pushes:

- `ghcr.io/<owner>/docker-dcm2niix:<dcm2niix-version>`
- `ghcr.io/<owner>/docker-dcm2niix:latest`
