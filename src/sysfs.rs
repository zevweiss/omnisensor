//! Utility code for interacting with sysfs files.

use std::path::{Path, PathBuf};
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::{
	sensor::{SensorIO, SensorType},
	types::*,
};

pub const PLATFORM_DEVICE_DIR: &str = "/sys/bus/platform/devices";

/// Read a bit of data from `fd` (at offset zero) and parse it as a `T`.
///
/// (While not strictly required, the expectation is that `fd` refers to a sysfs file that
/// contains a single integer.)
pub async fn read_and_parse<T: std::str::FromStr>(fd: &mut tokio::fs::File) -> ErrResult<T> {
	let mut buf = [0u8; 128];

	fd.rewind().await?;
	let n = fd.read(&mut buf).await?;
	let s = std::str::from_utf8(&buf[..n])?;

	if n == 0 || n >= buf.len() {
		return Err(err_invalid_data(format!("invalid sysfs data: {}", s)))
	}

	s.trim().parse::<T>()
		.map_err(|_| err_invalid_data(format!("invalid sysfs data: {}", s)))
}

/// Convenience function for cases where we expect a glob to produce exactly one match.
///
/// Returns Ok(_) if so, and Err(_) on multiple matches, no matches, or other errors.
pub fn get_single_glob_match(pattern: &str) -> ErrResult<PathBuf> {
	let mut matches = glob::glob(&pattern)?;
	let first = match matches.next() {
		Some(m) => m?,
		None => return Err(err_not_found("no match found")),
	};
	if matches.next().is_some() {
		Err(err_invalid_data("multiple matches found"))
	} else {
		Ok(first)
	}
}

/// For cases where we expect exactly one `.../hwmon/hwmonX` subdirectory of a given path.
///
/// This finds the one that's there, ensures there aren't any others, and returns it
/// (including whatever original prefix was passed).
pub fn get_single_hwmon_dir(path: &Path) -> ErrResult<PathBuf> {
	let pattern = path.join("hwmon/hwmon[0-9]*");
	get_single_glob_match(&pattern.to_string_lossy())
}

/// Summary info about a hwmon `*_input` file
pub struct HwmonFileInfo {
	/// The full absolute path of the file.
	pub abspath: PathBuf,

	/// The filename with `"_input"` stripped off, e.g. `"in1"`, `"temp3"`, etc.
	///
	/// The separate allocation here is unfortunate, but AFAIK we can't borrow from
	/// `abspath` without messing around with [`Pin`](std::pin::Pin) and such.
	pub base: String,

	/// The type of sensor the file reads from.
	pub kind: SensorType,

	/// Just the numeric part of `base`, parsed out (mostly just for sorting).
	pub idx: usize,
}

impl HwmonFileInfo {
	/// Construct a [`HwmonFileInfo`] from the given path.
	// ...though we don't actually check that it's absolute...should we?
	pub fn from_abspath(abspath: PathBuf) -> ErrResult<Self> {
		let mk_err = |msg| {
			err_invalid_data(format!("{}: {}", abspath.display(), msg))
		};

		let base = match abspath.file_name().map(|p| p.to_string_lossy()) {
			Some(s) => s.strip_suffix("_input")
				.ok_or_else(|| mk_err("no \"_input\" suffix"))?
				.to_string(),
			_ => return Err(err_invalid_data("no file name")),
		};

		let typetag = base.trim_end_matches(|c: char| c.is_ascii_digit());
		let Some(kind) = SensorType::from_hwmon_typetag(typetag) else {
			let msg = format!("unrecognized hwmon type tag '{}'", typetag);
			return Err(mk_err(&msg));
		};

		// unwrap because we're stripping a prefix
		// that we know is there
		let Ok(idx) = base.strip_prefix(typetag).unwrap().parse::<usize>() else {
			let msg = format!("couldn't parse index from '{}'", base);
			return Err(mk_err(&msg));
		};

		Ok(HwmonFileInfo {
			kind,
			idx,
			base,
			abspath,
		})
	}

	/// Return the label associated with the file.
	///
	/// If a corresponding `*_label` file exists, this is obtained by reading from it;
	/// otherwise it's just `self`'s [`base`](HwmonFileInfo::base) member.
	pub fn get_label(&self) -> ErrResult<String> {
		let labelpath = self.abspath.with_file_name(format!("{}_label", self.base));
		if labelpath.is_file() {
			Ok(std::fs::read_to_string(&labelpath).map(|s| s.trim().to_string())?)
		} else {
			Ok(self.base.clone())
		}
	}
}

/// Scan a device directoy fro hwmon `*_input` files.
///
/// Look in `devdir` for hwmon `*_input` files, optionally filtered to just those whose
/// names start with a certain prefix if `fileprefix` is [`Some`].
// fileprefix could just be a &str (with "" instead of None), but we might as well make
// it slightly more explicit.
pub fn scan_hwmon_input_files(devdir: &Path, fileprefix: Option<&str>) -> ErrResult<Vec<HwmonFileInfo>> {
	let hwmondir = get_single_hwmon_dir(devdir)?;
	let pattern = hwmondir.join(format!("{}*_input", fileprefix.unwrap_or("")));
	let mut info: Vec<_> = glob::glob(&pattern.to_string_lossy())?
		.filter_map(|g| {
			match g {
				Ok(abspath) => match HwmonFileInfo::from_abspath(abspath) {
					Ok(f) => Some(f),
					Err(e) => {
						eprintln!("Warning: {} (skipping)", e);
						None
					}
				},
				Err(e) => {
					eprintln!("Warning: error scanning {}, skipping entry: {}",
						  hwmondir.display(), e);
					None
				},
			}
		})
		.collect();

	info.sort_by_key(|info| (info.kind, info.idx));

	Ok(info)
}

/// Information for reading sensor data from sysfs.
pub struct SysfsSensorIO {
	/// A file descriptor to (asynchronously) read from.
	fd: tokio::fs::File,

	/// A multiplicative scale factor to convert raw sysfs file data to the desired
	/// units.
	///
	/// For example, the hwmon subsystem provides temperature readings in millidegrees
	/// C, so 0.001 here would convert that to degrees C as desired elsewhere.
	scale: f64,
}

impl SysfsSensorIO {
	/// Construct a [`SensorIO`] for a provided hwmon file.
	pub async fn new(file: &HwmonFileInfo) -> ErrResult<SensorIO> {
		let fd = tokio::fs::File::open(&file.abspath).await?;
		Ok(SensorIO::Sysfs(SysfsSensorIO {
			fd,
			scale: file.kind.hwmon_scale(),
		}))
	}

	/// Read a sensor sample (in natural units, e.g. degrees C instead of hwmon's
	/// millidegrees C).
	pub async fn read(&mut self) -> ErrResult<f64> {
		let ival = read_and_parse::<i32>(&mut self.fd).await?;
		Ok((ival as f64) * self.scale)
	}
}
