use tokio::io::{AsyncReadExt, AsyncSeekExt};

use crate::{
	sensor::{SensorIO, SensorType},
	types::*,
};

pub async fn read_and_parse<T: std::str::FromStr>(fd: &mut tokio::fs::File) -> ErrResult<T> {
	let mut buf = [0u8; 128];

	fd.rewind().await?;
	let n = fd.read(&mut buf).await?;
	let s = std::str::from_utf8(&buf[..n])?;

	s.trim().parse::<T>()
		.map_err(|_| Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData,
							  format!("invalid sysfs data: {}", s)))
			 as Box<dyn std::error::Error>)
}

// Convenience function for cases where we expect a glob to produce
// exactly one match.  Returns Ok(_) if so, and Err(_) on multiple
// matches, no matches, or other errors.
pub fn get_single_glob_match(pattern: &str) -> ErrResult<std::path::PathBuf> {
	let mut matches = glob::glob(&pattern)?;
	let first = match matches.next() {
		Some(m) => m?,
		None => return Err(Box::new(std::io::Error::new(std::io::ErrorKind::NotFound,
								"no match found"))),
	};
	if matches.next().is_some() {
		Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData,
						 "multiple matches found")))
	} else {
		Ok(first)
	}
}

// For cases where we expect exactly one .../hwmon/hwmonX subdirectory of a
// given path, this finds the one that's there, ensures there aren't any others,
// and returns it (including whatever original prefix was passed).
pub fn get_single_hwmon_dir(path: &str) -> ErrResult<std::path::PathBuf> {
	let pattern = format!("{}/hwmon/hwmon[0-9]*", path);
	get_single_glob_match(&pattern)
}

// Summary info about a hwmon *_input file
pub struct HwmonFileInfo {
	pub abspath: std::path::PathBuf,

	// Filename with  "_input" stripped off, e.g. "in1", "temp3", etc.
	// Allocation here is unfortunate, but AFAIK we can't borrow from
	// abspath without messing around with Pin and such.
	pub base: String,

	pub kind: SensorType,

	// Just the numeric part of base, parsed out (mostly just for sorting)
	pub idx: usize,
}

impl HwmonFileInfo {
	pub fn from_abspath(abspath: std::path::PathBuf) -> Option<Self> {
		let skip = || {
			eprintln!("Warning: don't know how to handle {}, skipping",
				  abspath.display());
		};

		let base = match abspath.file_name()?.to_str() {
			Some(s) => s.strip_suffix("_input")?
				.to_string(),
			_ => {
				skip();
				return None;
			},
		};

		let typetag = base.trim_end_matches(|c: char| c.is_ascii_digit());
		let Some(kind) = SensorType::from_hwmon_typetag(typetag) else {
			skip();
			return None;
		};

		// unwrap because we're stripping a prefix
		// that we know is there
		let Ok(idx) = base.strip_prefix(typetag).unwrap().parse::<usize>() else {
			skip();
			return None;
		};

		Some(HwmonFileInfo {
			kind,
			idx,
			base,
			abspath,
		})
	}

	pub fn get_label(&self) -> ErrResult<String> {
		let labelpath = self.abspath.with_file_name(format!("{}_label", self.base));
		if labelpath.is_file() {
			Ok(std::fs::read_to_string(&labelpath).map(|s| s.trim().to_string())?)
		} else {
			Ok(self.base.to_string())
		}
	}
}

// fileprefix could just be a &str (with "" instead of None), but
// meh...might as well make it slightly more explicit I guess?
pub fn scan_hwmon_input_files(devdir: &std::path::Path, fileprefix: Option<&str>) -> ErrResult<Vec<HwmonFileInfo>> {
	let hwmondir = get_single_hwmon_dir(&devdir.to_string_lossy())?;
	let pattern = hwmondir.join(format!("{}*_input", fileprefix.unwrap_or("")));
	let mut info: Vec<_> = glob::glob(&pattern.to_string_lossy())?
		.filter_map(|g| {
			match g {
				Ok(abspath) => HwmonFileInfo::from_abspath(abspath),
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

pub struct SysfsSensorIO {
	fd: tokio::fs::File,
	scale: f64,
}

impl SysfsSensorIO {
	pub async fn new(file: &HwmonFileInfo) -> ErrResult<SensorIO> {
		let fd = tokio::fs::File::open(&file.abspath).await?;
		Ok(SensorIO::Sysfs(SysfsSensorIO {
			fd,
			scale: file.kind.hwmon_scale(),
		}))
	}

	pub async fn read(&mut self) -> ErrResult<f64> {
		let ival = read_and_parse::<i32>(&mut self.fd).await?;
		Ok((ival as f64) * self.scale)
	}
}
