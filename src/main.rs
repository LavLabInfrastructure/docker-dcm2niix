use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fs::{self, File, Metadata};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::SystemTime;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;
type Matrix = [[f64; 4]; 3];
static TEMP_DIRECTORY_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Copy)]
enum Endian {
    Little,
    Big,
}

fn split_series_gradient_base(path: &Path) -> Option<PathBuf> {
    let (base, _) = split_image_path(path)?;
    let stem = base.file_name()?.to_str()?;
    let (shared_stem, index) = stem.rsplit_once('_')?;
    if index.parse::<usize>().ok()? == 0 {
        return None;
    }
    let shared_base = base.with_file_name(shared_stem);
    if sidecar(&shared_base, ".bvec").exists() || sidecar(&shared_base, ".bval").exists() {
        Some(shared_base)
    } else {
        None
    }
}

impl Endian {
    fn i16(self, b: &[u8]) -> i16 {
        let b = [b[0], b[1]];
        match self {
            Self::Little => i16::from_le_bytes(b),
            Self::Big => i16::from_be_bytes(b),
        }
    }

    fn i32(self, b: &[u8]) -> i32 {
        let b = b[..4].try_into().unwrap();
        match self {
            Self::Little => i32::from_le_bytes(b),
            Self::Big => i32::from_be_bytes(b),
        }
    }

    fn f32(self, b: &[u8]) -> f32 {
        f32::from_bits(self.i32(b) as u32)
    }

    fn f64(self, b: &[u8]) -> f64 {
        let b = b[..8].try_into().unwrap();
        match self {
            Self::Little => f64::from_le_bytes(b),
            Self::Big => f64::from_be_bytes(b),
        }
    }

    fn i64(self, b: &[u8]) -> i64 {
        let b = b[..8].try_into().unwrap();
        match self {
            Self::Little => i64::from_le_bytes(b),
            Self::Big => i64::from_be_bytes(b),
        }
    }

    fn u64(self, b: &[u8]) -> u64 {
        let b = b[..8].try_into().unwrap();
        match self {
            Self::Little => u64::from_le_bytes(b),
            Self::Big => u64::from_be_bytes(b),
        }
    }

    fn put_i16(self, b: &mut [u8], v: i16) {
        b[..2].copy_from_slice(&match self {
            Self::Little => v.to_le_bytes(),
            Self::Big => v.to_be_bytes(),
        });
    }

    fn put_f32(self, b: &mut [u8], v: f32) {
        b[..4].copy_from_slice(&match self {
            Self::Little => v.to_le_bytes(),
            Self::Big => v.to_be_bytes(),
        });
    }
}

fn fail(message: impl Into<String>) -> Box<dyn std::error::Error> {
    io::Error::new(io::ErrorKind::InvalidData, message.into()).into()
}

fn transform(m: &Matrix, p: [f64; 3]) -> [f64; 3] {
    std::array::from_fn(|r| m[r][3] + (0..3).map(|c| m[r][c] * p[c]).sum::<f64>())
}

fn determinant(m: &Matrix) -> f64 {
    m[0][0] * (m[1][1] * m[2][2] - m[1][2] * m[2][1])
        - m[0][1] * (m[1][0] * m[2][2] - m[1][2] * m[2][0])
        + m[0][2] * (m[1][0] * m[2][1] - m[1][1] * m[2][0])
}

fn inverse(m: &Matrix) -> Result<Matrix> {
    let d = determinant(m);
    if !d.is_finite() || d.abs() < 1e-12 {
        return Err(fail("singular NIfTI affine"));
    }
    let mut a = [[0.0; 4]; 3];
    for r in 0..3 {
        for c in 0..3 {
            let rows: Vec<_> = (0..3).filter(|&x| x != c).collect();
            let cols: Vec<_> = (0..3).filter(|&x| x != r).collect();
            let minor = m[rows[0]][cols[0]] * m[rows[1]][cols[1]]
                - m[rows[0]][cols[1]] * m[rows[1]][cols[0]];
            a[r][c] = if (r + c) % 2 == 0 {
                minor / d
            } else {
                -minor / d
            };
        }
        a[r][3] = -(0..3).map(|c| a[r][c] * m[c][3]).sum::<f64>();
    }
    Ok(a)
}

struct Image {
    bytes: Vec<u8>,
    endian: Endian,
    shape: [usize; 3],
    frames: usize,
    datatype: i16,
    bytes_per_voxel: usize,
    offset: usize,
    data_size: usize,
    slope: f64,
    intercept: f64,
    affine: Matrix,
    spacing: f64,
}

impl Image {
    fn read(path: &Path) -> Result<Option<Self>> {
        let Some(mut image) = Self::read_header(path)? else {
            return Ok(None);
        };
        image.load_data(path)?;
        Ok(Some(image))
    }

    fn read_header(path: &Path) -> Result<Option<Self>> {
        let mut bytes = vec![0; 352];
        if path.to_string_lossy().ends_with(".gz") {
            let mut child = Command::new("gzip")
                .arg("-dc")
                .arg("--")
                .arg(path)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()?;
            let mut stdout = child
                .stdout
                .take()
                .ok_or_else(|| fail("gzip stdout unavailable"))?;
            let header_result = stdout.read_exact(&mut bytes);
            drop(stdout);
            if header_result.is_err() {
                let output = child.wait_with_output()?;
                if !output.status.success() {
                    return Err(fail(format!(
                        "gzip failed on {}: {}",
                        path.display(),
                        String::from_utf8_lossy(&output.stderr)
                    )));
                }
                return Err(header_result.unwrap_err().into());
            }
            let _ = child.kill();
            child.wait()?;
        } else {
            File::open(path)?.read_exact(&mut bytes)?;
        }
        let endian = if i32::from_le_bytes(bytes[..4].try_into()?) == 348 {
            Endian::Little
        } else if i32::from_be_bytes(bytes[..4].try_into()?) == 348 {
            Endian::Big
        } else {
            return Err(fail("expected a NIfTI-1 image"));
        };
        if &bytes[344..348] != b"n+1\0" {
            return Err(fail("expected a single-file NIfTI-1 image"));
        }
        let n = endian.i16(&bytes[40..]) as usize;
        if (1..3).contains(&n) {
            return Ok(None);
        }
        if !(3..=7).contains(&n) {
            return Err(fail("invalid NIfTI dimension count"));
        }
        let mut dims = [1usize; 7];
        for (i, dim) in dims.iter_mut().enumerate().take(n) {
            let v = endian.i16(&bytes[42 + i * 2..]);
            if v <= 0 {
                return Err(fail("invalid NIfTI dimension"));
            }
            *dim = v as usize;
        }
        let shape = [dims[0], dims[1], dims[2]];
        if shape.contains(&1) {
            return Ok(None);
        }
        let frames = dims[3..]
            .iter()
            .try_fold(1usize, |a, b| a.checked_mul(*b))
            .ok_or_else(|| fail("NIfTI dimensions overflow"))?;
        let datatype = endian.i16(&bytes[70..]);
        let bitpix = endian.i16(&bytes[72..]);
        if bitpix <= 0 || bitpix % 8 != 0 {
            return Err(fail("invalid NIfTI bit depth"));
        }
        let bytes_per_voxel = bitpix as usize / 8;
        let offset_f = endian.f32(&bytes[108..]);
        if !offset_f.is_finite() || offset_f < 352.0 || offset_f.fract() != 0.0 {
            return Err(fail("invalid NIfTI data offset"));
        }
        let offset = offset_f as usize;
        let size = dims
            .iter()
            .try_fold(bytes_per_voxel, |a, b| a.checked_mul(*b))
            .and_then(|v| v.checked_add(offset))
            .ok_or_else(|| fail("NIfTI size overflow"))?;
        let raw_slope = endian.f32(&bytes[112..]) as f64;
        let raw_intercept = endian.f32(&bytes[116..]) as f64;
        let (slope, intercept) = if raw_slope == 0.0 {
            (1.0, 0.0)
        } else {
            (raw_slope, raw_intercept)
        };
        if !slope.is_finite() || !intercept.is_finite() {
            return Err(fail("invalid NIfTI intensity scaling"));
        }
        if endian.i16(&bytes[252..]) <= 0 && endian.i16(&bytes[254..]) <= 0 {
            eprintln!("Skipping {}: no spatial transform", path.display());
            return Ok(None);
        }
        let affine = affine(&bytes, endian)?;
        let lengths: [f64; 3] =
            std::array::from_fn(|c| (0..3).map(|r| affine[r][c].powi(2)).sum::<f64>().sqrt());
        if lengths.iter().any(|v| !v.is_finite() || *v <= 0.0)
            || affine.iter().flatten().any(|v| !v.is_finite())
        {
            return Err(fail("invalid NIfTI spatial transform"));
        }
        let spacing = lengths.iter().sum::<f64>() / 3.0;
        Ok(Some(Self {
            bytes,
            endian,
            shape,
            frames,
            datatype,
            bytes_per_voxel,
            offset,
            data_size: size,
            slope,
            intercept,
            affine,
            spacing,
        }))
    }

    fn load_data(&mut self, path: &Path) -> Result<()> {
        self.bytes = if path.to_string_lossy().ends_with(".gz") {
            let output = Command::new("gzip")
                .arg("-dc")
                .arg("--")
                .arg(path)
                .output()?;
            if !output.status.success() {
                return Err(fail(format!(
                    "gzip failed on {}: {}",
                    path.display(),
                    String::from_utf8_lossy(&output.stderr)
                )));
            }
            output.stdout
        } else {
            fs::read(path)?
        };
        if self.data_size > self.bytes.len() {
            return Err(fail("truncated NIfTI voxel data"));
        }
        Ok(())
    }

    fn eligible(&self) -> bool {
        if self.shape.contains(&1) {
            return false;
        }
        let lengths: [f64; 3] = std::array::from_fn(|c| {
            (0..3)
                .map(|r| self.affine[r][c].powi(2))
                .sum::<f64>()
                .sqrt()
        });
        let isotropic = lengths
            .iter()
            .all(|v| (v - self.spacing).abs() <= self.spacing * 0.01);
        let oblique = (0..3).any(|c| {
            let largest = (0..3)
                .map(|r| (self.affine[r][c] / lengths[c]).abs())
                .fold(0.0, f64::max);
            largest < (1.0f64.to_radians()).cos()
        });
        isotropic && oblique
    }

    fn validate_scalar(&self) -> Result<()> {
        let expected = match self.datatype {
            2 | 256 => 1,
            4 | 512 => 2,
            8 | 16 | 768 => 4,
            64 | 1024 | 1280 => 8,
            _ => {
                return Err(fail(format!(
                    "unsupported NIfTI datatype {} for resampling",
                    self.datatype
                )))
            }
        };
        if self.bytes_per_voxel != expected {
            return Err(fail("NIfTI datatype and bit depth disagree"));
        }
        let intent = self.endian.i16(&self.bytes[68..]);
        if matches!(intent, 1004..=1007) {
            return Err(fail(
                "matrix/vector-valued NIfTI images cannot be resampled as scalars",
            ));
        }
        inverse(&self.affine)?;
        Ok(())
    }

    fn voxel(&self, x: usize, y: usize, z: usize, frame: usize) -> f64 {
        let index = x + self.shape[0] * (y + self.shape[1] * (z + self.shape[2] * frame));
        let b = &self.bytes[self.offset + index * self.bytes_per_voxel..];
        let e = self.endian;
        let v = match self.datatype {
            2 => b[0] as f64,
            256 => (b[0] as i8) as f64,
            4 => e.i16(b) as f64,
            512 => (e.i16(b) as u16) as f64,
            8 => e.i32(b) as f64,
            768 => (e.i32(b) as u32) as f64,
            16 => e.f32(b) as f64,
            64 => e.f64(b),
            1024 => e.i64(b) as f64,
            1280 => e.u64(b) as f64,
            _ => unreachable!(),
        };
        v * self.slope + self.intercept
    }

    fn sample(&self, p: [f64; 3], frame: usize) -> f64 {
        let mut lo = [0usize; 3];
        let mut hi = [0usize; 3];
        let mut t = [0.0; 3];
        for i in 0..3 {
            let max = (self.shape[i] - 1) as f64;
            if p[i] < -1e-6 || p[i] > max + 1e-6 {
                return 0.0;
            }
            let value = p[i].clamp(0.0, max);
            lo[i] = value.floor() as usize;
            hi[i] = (lo[i] + 1).min(self.shape[i] - 1);
            t[i] = value - lo[i] as f64;
        }
        if matches!(self.endian.i16(&self.bytes[68..]), 1002 | 1003) {
            return self.voxel(
                if t[0] < 0.5 { lo[0] } else { hi[0] },
                if t[1] < 0.5 { lo[1] } else { hi[1] },
                if t[2] < 0.5 { lo[2] } else { hi[2] },
                frame,
            );
        }
        let mut result = 0.0;
        for z in 0..2 {
            for y in 0..2 {
                for x in 0..2 {
                    let weight = if x == 0 { 1.0 - t[0] } else { t[0] }
                        * if y == 0 { 1.0 - t[1] } else { t[1] }
                        * if z == 0 { 1.0 - t[2] } else { t[2] };
                    if weight != 0.0 {
                        result += weight
                            * self.voxel(
                                if x == 0 { lo[0] } else { hi[0] },
                                if y == 0 { lo[1] } else { hi[1] },
                                if z == 0 { lo[2] } else { hi[2] },
                                frame,
                            );
                    }
                }
            }
        }
        result
    }
}

fn affine(b: &[u8], e: Endian) -> Result<Matrix> {
    let sform = e.i16(&b[254..]);
    if sform > 0 {
        return Ok(std::array::from_fn(|r| {
            std::array::from_fn(|c| e.f32(&b[280 + r * 16 + c * 4..]) as f64)
        }));
    }
    if e.i16(&b[252..]) <= 0 {
        return Err(fail("NIfTI has no qform or sform"));
    }
    let mut q = [
        e.f32(&b[256..]) as f64,
        e.f32(&b[260..]) as f64,
        e.f32(&b[264..]) as f64,
    ];
    let sum = q.iter().map(|x| x * x).sum::<f64>();
    let a = if sum > 1.0 {
        let norm = sum.sqrt();
        for v in &mut q {
            *v /= norm;
        }
        0.0
    } else {
        (1.0 - sum).sqrt()
    };
    let [x, y, z] = q;
    let rot = [
        [
            a * a + x * x - y * y - z * z,
            2.0 * (x * y - a * z),
            2.0 * (x * z + a * y),
        ],
        [
            2.0 * (x * y + a * z),
            a * a + y * y - x * x - z * z,
            2.0 * (y * z - a * x),
        ],
        [
            2.0 * (x * z - a * y),
            2.0 * (y * z + a * x),
            a * a + z * z - x * x - y * y,
        ],
    ];
    let mut scale = [
        e.f32(&b[80..]) as f64,
        e.f32(&b[84..]) as f64,
        e.f32(&b[88..]) as f64,
    ];
    if e.f32(&b[76..]) < 0.0 {
        scale[2] = -scale[2];
    }
    Ok(std::array::from_fn(|r| {
        [
            rot[r][0] * scale[0],
            rot[r][1] * scale[1],
            rot[r][2] * scale[2],
            e.f32(&b[268 + r * 4..]) as f64,
        ]
    }))
}

struct Grid {
    shape: [usize; 3],
    affine: Matrix,
}

fn grids(image: &Image) -> Result<[Grid; 3]> {
    let mut min = [f64::INFINITY; 3];
    let mut max = [f64::NEG_INFINITY; 3];
    for mask in 0..8 {
        let corner = std::array::from_fn(|i| {
            if mask & (1 << i) == 0 {
                0.0
            } else {
                (image.shape[i] - 1) as f64
            }
        });
        let world = transform(&image.affine, corner);
        for i in 0..3 {
            min[i] = min[i].min(world[i]);
            max[i] = max[i].max(world[i]);
        }
    }
    let mut world_shape = [0usize; 3];
    for i in 0..3 {
        let n = ((max[i] - min[i]) / image.spacing).ceil() + 1.0;
        if !n.is_finite() || !(1.0..=32767.0).contains(&n) {
            return Err(fail("resampled NIfTI dimensions exceed the NIfTI-1 limit"));
        }
        world_shape[i] = n as usize;
    }
    let grid = |axes: [usize; 3]| {
        let mut a = [[0.0; 4]; 3];
        for r in 0..3 {
            a[r][3] = min[r];
        }
        for c in 0..3 {
            a[axes[c]][c] = image.spacing;
        }
        Grid {
            shape: axes.map(|i| world_shape[i]),
            affine: a,
        }
    };
    Ok([grid([0, 1, 2]), grid([1, 2, 0]), grid([0, 2, 1])])
}

fn write_image(image: &Image, grid: &Grid, path: &Path, compressed: bool) -> Result<()> {
    let mut header = image.bytes[..352].to_vec();
    let e = image.endian;
    let double = matches!(image.datatype, 8 | 64 | 768 | 1024 | 1280);
    for i in 0..3 {
        e.put_i16(&mut header[42 + 2 * i..], grid.shape[i] as i16);
        e.put_f32(&mut header[80 + 4 * i..], image.spacing as f32);
    }
    e.put_i16(&mut header[70..], if double { 64 } else { 16 });
    e.put_i16(&mut header[72..], if double { 64 } else { 32 });
    e.put_f32(&mut header[76..], 1.0);
    e.put_f32(&mut header[108..], 352.0);
    e.put_f32(&mut header[112..], 1.0);
    e.put_f32(&mut header[116..], 0.0);
    e.put_f32(&mut header[124..], 0.0);
    e.put_f32(&mut header[128..], 0.0);
    header[39] = 0;
    e.put_i16(&mut header[74..], 0);
    e.put_i16(&mut header[120..], 0);
    header[122] = 0;
    e.put_f32(&mut header[132..], 0.0);
    let sform_code = e.i16(&header[254..]);
    let source_code = if sform_code > 0 {
        sform_code
    } else {
        e.i16(&header[252..]).max(1)
    };
    e.put_i16(&mut header[252..], 0);
    e.put_i16(&mut header[254..], source_code);
    for r in 0..3 {
        for c in 0..4 {
            e.put_f32(
                &mut header[280 + r * 16 + c * 4..],
                grid.affine[r][c] as f32,
            );
        }
    }
    header[348..352].fill(0);
    let file = File::create(path)?;
    let mut child = if compressed {
        Some(
            Command::new("gzip")
                .arg("-c")
                .stdin(Stdio::piped())
                .stdout(Stdio::from(file.try_clone()?))
                .spawn()?,
        )
    } else {
        None
    };
    let mut out: Box<dyn Write> = if let Some(ref mut child) = child {
        Box::new(
            child
                .stdin
                .take()
                .ok_or_else(|| fail("gzip stdin unavailable"))?,
        )
    } else {
        Box::new(io::BufWriter::new(file))
    };
    out.write_all(&header)?;
    let inv = inverse(&image.affine)?;
    let mut row = Vec::with_capacity(grid.shape[0] * if double { 8 } else { 4 });
    for frame in 0..image.frames {
        for z in 0..grid.shape[2] {
            for y in 0..grid.shape[1] {
                row.clear();
                for x in 0..grid.shape[0] {
                    let pos = transform(
                        &inv,
                        transform(&grid.affine, [x as f64, y as f64, z as f64]),
                    );
                    let v = image.sample(pos, frame);
                    if double {
                        row.extend_from_slice(&match e {
                            Endian::Little => v.to_le_bytes(),
                            Endian::Big => v.to_be_bytes(),
                        });
                    } else {
                        row.extend_from_slice(&match e {
                            Endian::Little => (v as f32).to_le_bytes(),
                            Endian::Big => (v as f32).to_be_bytes(),
                        });
                    }
                }
                out.write_all(&row)?;
            }
        }
    }
    out.flush()?;
    drop(out);
    if let Some(mut child) = child {
        if !child.wait()?.success() {
            return Err(fail("gzip failed while writing NIfTI"));
        }
    }
    Ok(())
}

struct TemporaryFiles {
    directory: PathBuf,
    files: Vec<PathBuf>,
}

impl TemporaryFiles {
    fn new(parent: &Path) -> Result<Self> {
        use std::os::unix::fs::DirBuilderExt;
        loop {
            let id = TEMP_DIRECTORY_COUNTER.fetch_add(1, Ordering::Relaxed);
            let directory = parent.join(format!(".dcm2niix-orient-{}-{id}", std::process::id()));
            match fs::DirBuilder::new().mode(0o700).create(&directory) {
                Ok(()) => {
                    return Ok(Self {
                        directory,
                        files: Vec::new(),
                    })
                }
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(e.into()),
            }
        }
    }

    fn allocate(&mut self) -> Result<PathBuf> {
        let path = self.directory.join(self.files.len().to_string());
        File::options().write(true).create_new(true).open(&path)?;
        self.files.push(path.clone());
        Ok(path)
    }
}

impl Drop for TemporaryFiles {
    fn drop(&mut self) {
        for path in &self.files {
            if let Err(e) = fs::remove_file(path) {
                if e.kind() != io::ErrorKind::NotFound {
                    eprintln!("Could not remove temporary file {}: {e}", path.display());
                }
            }
        }
        if let Err(e) = fs::remove_dir(&self.directory) {
            eprintln!(
                "Could not remove temporary directory {}: {e}",
                self.directory.display()
            );
        }
    }
}

fn sidecar(base: &Path, suffix: &str) -> PathBuf {
    let mut name = base.as_os_str().to_os_string();
    name.push(suffix);
    PathBuf::from(name)
}

fn split_image_path(path: &Path) -> Option<(PathBuf, &'static str)> {
    let name = path.file_name()?.to_str()?;
    let (stem, ext) = if let Some(stem) = name.strip_suffix(".nii.gz") {
        (stem, ".nii.gz")
    } else {
        (name.strip_suffix(".nii")?, ".nii")
    };
    Some((path.with_file_name(stem), ext))
}

fn rotate_bvecs(text: &str, image: &Image, grid: &Grid) -> Result<String> {
    let rows: Vec<Vec<f64>> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|line| line.split_whitespace().map(str::parse::<f64>).collect())
        .collect::<std::result::Result<_, _>>()?;
    if rows.len() != 3
        || rows
            .iter()
            .any(|r| r.len() != image.frames || r.iter().any(|v| !v.is_finite()))
    {
        return Err(fail(
            "bvec must contain three rows with one finite value per volume",
        ));
    }
    let lengths: [f64; 3] = std::array::from_fn(|c| {
        (0..3)
            .map(|r| image.affine[r][c].powi(2))
            .sum::<f64>()
            .sqrt()
    });
    let old: [[f64; 3]; 3] =
        std::array::from_fn(|r| std::array::from_fn(|c| image.affine[r][c] / lengths[c]));
    for a in 0..3 {
        for b in a + 1..3 {
            let dot = (0..3).map(|r| old[r][a] * old[r][b]).sum::<f64>();
            if dot.abs() > 0.001 {
                return Err(fail(
                    "cannot rotate diffusion gradients for a sheared affine",
                ));
            }
        }
    }
    let mut output = [Vec::new(), Vec::new(), Vec::new()];
    for i in 0..image.frames {
        let mut v = [rows[0][i], rows[1][i], rows[2][i]];
        // FSL b-vectors use a flipped first voxel axis for positive-determinant affines.
        if determinant(&image.affine) > 0.0 {
            v[0] = -v[0];
        }
        let world: [f64; 3] = std::array::from_fn(|r| (0..3).map(|c| old[r][c] * v[c]).sum());
        let mut rotated: [f64; 3] = std::array::from_fn(|c| {
            (0..3)
                .map(|r| grid.affine[r][c] / image.spacing * world[r])
                .sum()
        });
        if determinant(&grid.affine) > 0.0 {
            rotated[0] = -rotated[0];
        }
        for r in 0..3 {
            output[r].push(format!("{:.10}", rotated[r]));
        }
    }
    Ok(output
        .iter()
        .map(|r| r.join(" "))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n")
}

fn process_image(path: &Path) -> Result<()> {
    if let Some(shared_base) = split_series_gradient_base(path) {
        return Err(fail(format!(
            "cannot resample split diffusion series with shared gradients: {}",
            shared_base.display()
        )));
    }
    let Some(mut image) = Image::read_header(path)? else {
        return Ok(());
    };
    if !image.eligible() {
        return Ok(());
    }
    image.load_data(path)?;
    image.validate_scalar()?;
    let grids = grids(&image)?;
    let (base, extension) = split_image_path(path).ok_or_else(|| fail("invalid NIfTI filename"))?;
    let dir = path
        .parent()
        .ok_or_else(|| fail("NIfTI output has no parent directory"))?;
    let bases = [
        base.clone(),
        sidecar(&base, "_sagittal"),
        sidecar(&base, "_coronal"),
    ];
    let destinations = [
        path.to_path_buf(),
        sidecar(&bases[1], extension),
        sidecar(&bases[2], extension),
    ];
    for dest in &destinations[1..] {
        if dest.exists() {
            return Err(fail(format!("refusing to overwrite {}", dest.display())));
        }
    }

    let json = sidecar(&base, ".json");
    let bvec = sidecar(&base, ".bvec");
    let bval = sidecar(&base, ".bval");
    let gradients = if bvec.exists() {
        Some(fs::read_to_string(&bvec)?)
    } else {
        None
    };
    let mut temp = TemporaryFiles::new(dir)?;
    let mut moves: Vec<(PathBuf, PathBuf)> = Vec::new();
    for (i, grid) in grids.iter().enumerate() {
        let target = temp.allocate()?;
        write_image(&image, grid, &target, extension == ".nii.gz")?;
        moves.push((target, destinations[i].clone()));
        if let Some(ref gradients) = gradients {
            let dest = sidecar(&bases[i], ".bvec");
            if i != 0 && dest.exists() {
                return Err(fail(format!("refusing to overwrite {}", dest.display())));
            }
            let target = temp.allocate()?;
            fs::write(&target, rotate_bvecs(gradients, &image, grid)?)?;
            moves.push((target, dest));
        }
        if i != 0 && bval.exists() {
            let dest = sidecar(&bases[i], ".bval");
            if dest.exists() {
                return Err(fail(format!("refusing to overwrite {}", dest.display())));
            }
            let target = temp.allocate()?;
            fs::copy(&bval, &target)?;
            moves.push((target, dest));
        }
        if json.exists() {
            let dest = sidecar(&bases[i], ".json");
            if i != 0 && dest.exists() {
                return Err(fail(format!("refusing to overwrite {}", dest.display())));
            }
            let target = temp.allocate()?;
            let output = File::create(&target)?;
            let status = Command::new("jq")
                .args(["del(.PhaseEncodingDirection, .PhaseEncodingAxis, .SliceEncodingDirection, .SliceTiming, .SliceEncodingOrder) | . + {SpatialResampling: \"Resampled to cardinal axes; acquisition metadata describes the source DICOM\"}"])
                .arg(&json).stdout(Stdio::from(output)).status()?;
            if !status.success() {
                return Err(fail(format!("could not update {}", json.display())));
            }
            moves.push((target, dest));
        }
    }
    if json.exists() {
        let dest = sidecar(&base, "_source.json");
        if dest.exists() {
            return Err(fail(format!("refusing to overwrite {}", dest.display())));
        }
        let target = temp.allocate()?;
        fs::copy(&json, &target)?;
        moves.push((target, dest));
    }
    // Replace the source image last, after all derivative files were prepared.
    moves.rotate_left(1);
    for (source, dest) in moves {
        fs::rename(source, dest)?;
    }
    eprintln!(
        "Resampled {} into axial, sagittal and coronal images",
        path.display()
    );
    Ok(())
}

#[derive(PartialEq, Eq)]
struct FileState {
    len: u64,
    modified: Option<SystemTime>,
    changed: i64,
    changed_ns: i64,
    inode: u64,
}

fn state(meta: &Metadata) -> FileState {
    use std::os::unix::fs::MetadataExt;
    FileState {
        len: meta.len(),
        modified: meta.modified().ok(),
        changed: meta.ctime(),
        changed_ns: meta.ctime_nsec(),
        inode: meta.ino(),
    }
}

fn images(dir: &Path) -> Result<HashMap<PathBuf, FileState>> {
    let mut found = HashMap::new();
    if !dir.exists() {
        return Ok(found);
    }
    let mut pending = vec![dir.to_path_buf()];
    while let Some(directory) = pending.pop() {
        for item in fs::read_dir(directory)? {
            let item = item?;
            let path = item.path();
            let file_type = item.file_type()?;
            if file_type.is_dir() {
                pending.push(path);
            } else if file_type.is_file() && split_image_path(&path).is_some() {
                let meta = item.metadata()?;
                found.insert(path, state(&meta));
            }
        }
    }
    Ok(found)
}

fn run() -> Result<ExitCode> {
    let mut args: Vec<OsString> = env::args_os().skip(1).collect();
    if args
        .first()
        .is_some_and(|a| a == "dcm2niix" || a == "/usr/local/bin/dcm2niix")
    {
        args.remove(0);
    }
    let mut output = None;
    for (i, arg) in args.iter().enumerate() {
        if arg == "-o" {
            output = Some(PathBuf::from(
                args.get(i + 1)
                    .ok_or_else(|| fail("-o requires an output directory"))?,
            ));
        }
    }
    let previous = match &output {
        Some(dir) => images(dir)?,
        None => HashMap::new(),
    };
    let status = Command::new("/usr/local/bin/dcm2niix-upstream")
        .args(&args)
        .status()?;
    if !status.success() {
        return Ok(ExitCode::from(
            status.code().unwrap_or(1).clamp(1, 255) as u8
        ));
    }
    if let Some(dir) = output {
        let mut changed: Vec<_> = images(&dir)?
            .into_iter()
            .filter(|(path, meta)| previous.get(path) != Some(meta))
            .map(|(path, _)| path)
            .collect();
        changed.sort();
        for path in changed {
            process_image(&path)?;
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn main() -> ExitCode {
    match run() {
        Ok(status) => status,
        Err(e) => {
            eprintln!("dcm2niix orientation processing failed: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image(affine: Matrix) -> Image {
        let mut bytes = vec![0; 352 + 8 * 4];
        bytes[344..348].copy_from_slice(b"n+1\0");
        for i in 0..8 {
            bytes[352 + i * 4..356 + i * 4].copy_from_slice(&(i as f32).to_le_bytes());
        }
        Image {
            bytes,
            endian: Endian::Little,
            shape: [2, 2, 2],
            frames: 1,
            datatype: 16,
            bytes_per_voxel: 4,
            offset: 352,
            data_size: 384,
            slope: 1.0,
            intercept: 0.0,
            affine,
            spacing: 1.0,
        }
    }

    fn oblique() -> Matrix {
        let c = 0.5f64.sqrt();
        [[c, -c, 0.0, 10.0], [c, c, 0.0, -4.0], [0.0, 0.0, 1.0, 3.0]]
    }

    #[test]
    fn inverse_round_trip() {
        let a = oblique();
        let p = [1.5, 3.2, 7.7];
        let q = transform(&inverse(&a).unwrap(), transform(&a, p));
        for i in 0..3 {
            assert!((p[i] - q[i]).abs() < 1e-10);
        }
    }

    #[test]
    fn only_oblique_isotropic_images_qualify() {
        assert!(image(oblique()).eligible());
        assert!(!image([
            [0.0, -1.0, 0.0, 0.0],
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0]
        ])
        .eligible());
        let mut anisotropic = image(oblique());
        for r in 0..3 {
            anisotropic.affine[r][2] *= 2.0;
        }
        anisotropic.spacing = 4.0 / 3.0;
        assert!(!anisotropic.eligible());
        let mut small_angle = image(oblique());
        let a = 0.5f64.to_radians();
        small_angle.affine = [
            [a.cos(), -a.sin(), 0.0, 0.0],
            [a.sin(), a.cos(), 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ];
        assert!(!small_angle.eligible());
    }

    #[test]
    fn grids_cover_source_and_use_the_expected_axes() {
        let image = image(oblique());
        let grids = grids(&image).unwrap();
        assert_eq!(grids[0].shape, [3, 3, 2]);
        assert_eq!(grids[1].shape, [3, 2, 3]);
        assert_eq!(grids[2].shape, [3, 2, 3]);
        assert!(
            grids[0].affine[0][0] > 0.0
                && grids[0].affine[1][1] > 0.0
                && grids[0].affine[2][2] > 0.0
        );
        assert!(
            grids[1].affine[1][0] > 0.0
                && grids[1].affine[2][1] > 0.0
                && grids[1].affine[0][2] > 0.0
        );
        assert!(
            grids[2].affine[0][0] > 0.0
                && grids[2].affine[2][1] > 0.0
                && grids[2].affine[1][2] > 0.0
        );
        for mask in 0..8 {
            let p = std::array::from_fn(|i| ((mask >> i) & 1) as f64);
            let world = transform(&image.affine, p);
            let out = transform(&inverse(&grids[0].affine).unwrap(), world);
            for (i, v) in out.iter().enumerate() {
                assert!(*v >= -1e-9 && *v <= (grids[0].shape[i] - 1) as f64 + 1e-9);
            }
        }
    }

    #[test]
    fn interpolation_and_label_sampling() {
        let mut img = image(oblique());
        assert!((img.sample([0.25, 0.5, 0.75], 0) - 4.25).abs() < 1e-10);
        assert_eq!(img.sample([-0.01, 0.0, 0.0], 0), 0.0);
        img.endian.put_i16(&mut img.bytes[68..], 1002);
        assert_eq!(img.sample([0.25, 0.5, 0.75], 0), 6.0);
    }

    #[test]
    fn decodes_and_interpolates_64_bit_integer_voxels() {
        for endian in [Endian::Little, Endian::Big] {
            let mut img = image(oblique());
            img.endian = endian;
            img.bytes_per_voxel = 8;
            img.bytes.resize(352 + 8 * 8, 0);
            img.slope = 2.0;
            img.intercept = 3.0;

            img.datatype = 1024;
            for i in 0..8 {
                let value = -40i64 + i as i64 * 10;
                let encoded = match endian {
                    Endian::Little => value.to_le_bytes(),
                    Endian::Big => value.to_be_bytes(),
                };
                img.bytes[352 + i * 8..360 + i * 8].copy_from_slice(&encoded);
            }
            assert_eq!(img.voxel(0, 0, 0, 0), -77.0);
            assert_eq!(img.voxel(1, 1, 1, 0), 63.0);
            assert_eq!(img.sample([0.5, 0.5, 0.5], 0), -7.0);

            img.datatype = 1280;
            for i in 0..8 {
                let value = 9_000_000_000u64 + i as u64 * 10;
                let encoded = match endian {
                    Endian::Little => value.to_le_bytes(),
                    Endian::Big => value.to_be_bytes(),
                };
                img.bytes[352 + i * 8..360 + i * 8].copy_from_slice(&encoded);
            }
            assert_eq!(img.voxel(0, 0, 0, 0), 18_000_000_003.0);
            assert_eq!(img.voxel(1, 1, 1, 0), 18_000_000_143.0);
            assert_eq!(img.sample([0.5, 0.5, 0.5], 0), 18_000_000_073.0);

            let min = match endian {
                Endian::Little => i64::MIN.to_le_bytes(),
                Endian::Big => i64::MIN.to_be_bytes(),
            };
            let max = match endian {
                Endian::Little => u64::MAX.to_le_bytes(),
                Endian::Big => u64::MAX.to_be_bytes(),
            };
            assert_eq!(endian.i64(&min), i64::MIN);
            assert_eq!(endian.u64(&max), u64::MAX);
        }
    }

    #[test]
    fn rotates_fsl_gradients_for_each_affine_handedness() {
        let c = 0.5f64.sqrt();
        let mut mirrored = oblique();
        for row in &mut mirrored {
            row[0] = -row[0];
        }
        for img in [image(oblique()), image(mirrored)] {
            let grids = grids(&img).unwrap();
            for grid in &grids {
                let result = rotate_bvecs("1\n0\n0\n", &img, grid).unwrap();
                let rows: Vec<f64> = result.lines().map(|x| x.parse().unwrap()).collect();
                let mut v = [rows[0], rows[1], rows[2]];
                if determinant(&grid.affine) > 0.0 {
                    v[0] = -v[0];
                }
                let world = transform(&grid.affine, v);
                let origin = transform(&grid.affine, [0.0; 3]);
                assert!((world[0] - origin[0] + c).abs() < 1e-8);
                assert!((world[1] - origin[1] + c).abs() < 1e-8);
                assert!((world[2] - origin[2]).abs() < 1e-8);
            }
            assert!(rotate_bvecs("1 2\n0 0\n0 0\n", &img, &grids[0]).is_err());
        }
        let mut sheared = image(oblique());
        sheared.affine[0][1] += 0.1;
        assert!(rotate_bvecs("1\n0\n0\n", &sheared, &grids(&sheared).unwrap()[0]).is_err());
    }

    #[test]
    fn parses_qform_with_translation_and_negative_qfac() {
        let mut b = [0u8; 352];
        let e = Endian::Little;
        e.put_i16(&mut b[252..], 1);
        e.put_f32(&mut b[76..], -1.0);
        for i in 0..3 {
            e.put_f32(&mut b[80 + i * 4..], (i + 1) as f32);
            e.put_f32(&mut b[268 + i * 4..], (10 + i) as f32);
        }
        let a = affine(&b, e).unwrap();
        assert_eq!(
            a,
            [
                [1.0, 0.0, 0.0, 10.0],
                [0.0, 2.0, 0.0, 11.0],
                [0.0, 0.0, -3.0, 12.0]
            ]
        );
    }

    fn write_fixture(path: &Path) {
        let e = Endian::Little;
        let mut b = vec![0u8; 352 + 16 * 4];
        b[..4].copy_from_slice(&348i32.to_le_bytes());
        b[344..348].copy_from_slice(b"n+1\0");
        e.put_i16(&mut b[40..], 4);
        for i in 0..4 {
            e.put_i16(&mut b[42 + i * 2..], 2);
        }
        e.put_i16(&mut b[70..], 16);
        e.put_i16(&mut b[72..], 32);
        e.put_f32(&mut b[108..], 352.0);
        e.put_f32(&mut b[112..], 2.0);
        e.put_f32(&mut b[116..], 3.0);
        e.put_i16(&mut b[252..], 4);
        e.put_i16(&mut b[254..], 1);
        let a = oblique();
        for r in 0..3 {
            for c in 0..4 {
                e.put_f32(&mut b[280 + r * 16 + c * 4..], a[r][c] as f32);
            }
        }
        for i in 0..16 {
            b[352 + i * 4..356 + i * 4].copy_from_slice(&(i as f32).to_le_bytes());
        }
        fs::write(path, b).unwrap();
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new(name: &str) -> Self {
            let path = env::temp_dir().join(format!("dcm2niix-test-{}-{name}", std::process::id()));
            fs::create_dir(&path).unwrap();
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            fs::remove_dir_all(&self.0).unwrap();
        }
    }

    #[test]
    fn processes_4d_images_and_updates_sidecars() {
        let dir = TestDir::new("4d");
        let input = dir.0.join("scan.nii");
        write_fixture(&input);
        fs::write(dir.0.join("scan.bvec"), "0 1\n0 0\n0 0\n").unwrap();
        fs::write(dir.0.join("scan.bval"), "0 1000\n").unwrap();
        let json = r#"{"PhaseEncodingDirection":"j","PhaseEncodingAxis":1,"SliceTiming":[0,1],"Manufacturer":"test"}"#;
        fs::write(dir.0.join("scan.json"), json).unwrap();
        process_image(&input).unwrap();
        let axial = Image::read(&input).unwrap().unwrap();
        let sagittal = Image::read(&dir.0.join("scan_sagittal.nii"))
            .unwrap()
            .unwrap();
        let coronal = Image::read(&dir.0.join("scan_coronal.nii"))
            .unwrap()
            .unwrap();
        assert_eq!(axial.frames, 2);
        assert_eq!(axial.endian.i16(&axial.bytes[254..]), 1);
        for frame in 0..2 {
            for z in 0..axial.shape[2] {
                for y in 0..axial.shape[1] {
                    for x in 0..axial.shape[0] {
                        assert_eq!(axial.voxel(x, y, z, frame), sagittal.voxel(y, z, x, frame));
                        assert_eq!(axial.voxel(x, y, z, frame), coronal.voxel(x, z, y, frame));
                    }
                }
            }
        }
        assert!(axial.voxel(1, 1, 0, 1) > axial.voxel(1, 1, 0, 0));
        for suffix in ["", "_sagittal", "_coronal"] {
            assert!(dir.0.join(format!("scan{suffix}.bvec")).exists());
            assert_eq!(
                fs::read_to_string(dir.0.join(format!("scan{suffix}.bval"))).unwrap(),
                "0 1000\n"
            );
            let metadata = fs::read_to_string(dir.0.join(format!("scan{suffix}.json"))).unwrap();
            assert!(!metadata.contains("PhaseEncodingDirection"));
            assert!(!metadata.contains("PhaseEncodingAxis"));
            assert!(!metadata.contains("SliceTiming"));
            assert!(metadata.contains("SpatialResampling"));
        }
        assert_eq!(
            fs::read_to_string(dir.0.join("scan_source.json")).unwrap(),
            json
        );
    }

    #[test]
    fn refuses_derivative_name_collisions_without_changing_source() {
        let dir = TestDir::new("collision");
        let input = dir.0.join("scan.nii");
        write_fixture(&input);
        let original = fs::read(&input).unwrap();
        fs::write(dir.0.join("scan_sagittal.nii"), b"existing").unwrap();
        assert!(process_image(&input).is_err());
        assert_eq!(fs::read(input).unwrap(), original);
        assert_eq!(
            fs::read(dir.0.join("scan_sagittal.nii")).unwrap(),
            b"existing"
        );
        assert!(!dir.0.join("scan_coronal.nii").exists());
    }

    #[test]
    fn leaves_cardinal_and_anisotropic_images_unchanged() {
        let dir = TestDir::new("unchanged");
        let input = dir.0.join("scan.nii");
        for cardinal in [true, false] {
            write_fixture(&input);
            let mut bytes = fs::read(&input).unwrap();
            let affine = if cardinal {
                [
                    [1.0, 0.0, 0.0, 0.0],
                    [0.0, 1.0, 0.0, 0.0],
                    [0.0, 0.0, 1.0, 0.0],
                ]
            } else {
                let mut a = oblique();
                a[2][2] = 2.0;
                a
            };
            for r in 0..3 {
                for c in 0..4 {
                    Endian::Little.put_f32(&mut bytes[280 + r * 16 + c * 4..], affine[r][c] as f32);
                }
            }
            fs::write(&input, &bytes).unwrap();
            process_image(&input).unwrap();
            assert_eq!(fs::read(&input).unwrap(), bytes);
            assert!(!dir.0.join("scan_sagittal.nii").exists());
            assert!(!dir.0.join("scan_coronal.nii").exists());
        }
    }

    #[test]
    fn skips_voxel_loading_for_ineligible_images() {
        let dir = TestDir::new("header-only");
        let input = dir.0.join("scan.nii");
        write_fixture(&input);
        let mut bytes = fs::read(&input).unwrap();
        let cardinal = [
            [1.0, 0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0, 0.0],
            [0.0, 0.0, 1.0, 0.0],
        ];
        for r in 0..3 {
            for c in 0..4 {
                Endian::Little.put_f32(&mut bytes[280 + r * 16 + c * 4..], cardinal[r][c] as f32);
            }
        }
        bytes.truncate(352);
        fs::write(&input, &bytes).unwrap();
        process_image(&input).unwrap();
        assert_eq!(fs::read(&input).unwrap(), bytes);
    }

    #[test]
    fn rejects_split_diffusion_images_with_shared_gradients() {
        let dir = TestDir::new("split-diffusion");
        let input = dir.0.join("scan_1.nii");
        write_fixture(&input);
        let original = fs::read(&input).unwrap();
        fs::write(dir.0.join("scan.bvec"), "1 0\n0 1\n0 0\n").unwrap();
        assert!(process_image(&input).is_err());
        assert_eq!(fs::read(&input).unwrap(), original);
        assert!(!dir.0.join("scan_1_sagittal.nii").exists());
    }

    #[test]
    fn images_includes_new_and_changed_nested_niftis() {
        let dir = TestDir::new("nested");
        let nested = dir.0.join("subject");
        let deeper = nested.join("session");
        fs::create_dir_all(&deeper).unwrap();
        let changed = nested.join("scan.nii");
        write_fixture(&changed);
        let before = images(&dir.0).unwrap();
        write_fixture(&changed);
        let added = deeper.join("scan.nii.gz");
        fs::write(&added, b"fixture").unwrap();
        let after = images(&dir.0).unwrap();
        assert!(before.get(&changed) != after.get(&changed));
        assert!(after.contains_key(&added));
    }
}
