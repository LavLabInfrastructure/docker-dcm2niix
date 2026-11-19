# docker-dcm2niix

A small image containing the upstream x86-64 dcm2niix binary and an
orientation wrapper. Download and Rust build tools stay in build stages;
the runtime is Debian slim because the upstream binary requires glibc.

## XNAT output

The existing XNAT command line can stay as written:

```text
dcm2niix -z [COMPRESS] -f [FILENAME] [BIDS] [OTHER_OPTIONS] -o /output /input
```

For each converted 3D or higher-dimensional image, the wrapper checks the
NIfTI spatial transform. If all three voxel spacings are within 1% of their
mean and any voxel axis is more than 1 degree from the nearest cardinal axis,
it resamples the volume to patient-aligned grids. With the default filename
and gzip compression, the output is:

- `nifti.nii.gz` (axial)
- `nifti_sagittal.nii.gz`
- `nifti_coronal.nii.gz`

Other filename formats and uncompressed `.nii` files work as well. Volumes
that are not isotropic and oblique are left as dcm2niix wrote them, as are 2D
images and images without a spatial transform. The wrapper processes only
files created or changed by the current conversion, and requires an explicit
`-o` directory to perform this post-processing.

Resampling covers the original voxel centers and uses trilinear interpolation
(nearest-neighbor for NIfTI label intents). It changes voxel values and stores
the derivatives as floating-point data; these are not substitutes for the
original acquisition for quantitative analysis. Spatial NIfTI extensions
are not carried into the derivatives. Higher dimensions are preserved.

When present, FSL `.bvec` files are rotated for each output and `.bval` files
are copied. Diffusion images with a sheared affine, vector/matrix-valued
NIfTIs, unsupported voxel types, and filename collisions cause an explicit
error instead of producing potentially misleading derivatives. BIDS JSON
sidecars are copied for all three images, with phase/slice encoding axis,
direction, and slice timing fields removed because they no longer describe the
resampled axes. Split-series diffusion outputs with shared gradient sidecars
are rejected instead of resampling without rotating the gradients. The
original acquisition JSON is retained as `<filename>_source.json`.
Review acquisition metadata and validate derivatives for the intended
downstream use.

## Build

```sh
docker build --build-arg DCM2NIIX_VERSION=v1.0.20261029 -t dcm2niix .
docker run --rm dcm2niix -h
```

For the wrapper tests, run:

```sh
rustc --edition=2021 --test src/main.rs -o /tmp/dcm2niix-tests
/tmp/dcm2niix-tests
```

The tests need `jq` and `gzip` installed.

## Automated monthly updates

A scheduled GitHub Actions workflow (`.github/workflows/monthly-dcm2niix-update.yml`) checks
the latest `rordenlab/dcm2niix` release every month.

If an image tag for that release does not already exist in GHCR, the workflow builds and pushes:

- `ghcr.io/<owner>/docker-dcm2niix:<dcm2niix-version>`
- `ghcr.io/<owner>/docker-dcm2niix:latest`

The workflow skips publishing when the upstream version tag already exists,
so changing this repository does not by itself update an existing `latest`
image.
