use std::{
	collections::{HashMap, HashSet},
	time::Duration,
};
use dbus::arg::RefArg;
use glob;
use phf::phf_set;

use crate::{
	types::{ErrResult, FilterSet},
	i2c::{
		I2CDeviceParams,
		I2CDeviceMap,
		get_i2cdev,
	},
	powerstate::PowerState,
	sensor,
	sensor::{
		DBusSensorMap,
		Sensor,
		SensorConfig,
		SensorConfigMap,
		SensorType,
	},
};

#[derive(Debug)]
pub struct HwmonSensorConfig {
	names: Vec<String>,
	name_overrides: HashMap<String, String>,
	i2c: I2CDeviceParams,
	poll_interval: Duration,
	power_state: PowerState,
	enabled_labels: FilterSet<String>,
}

static PSU_TYPES: phf::Set<&'static str> = phf_set! {
	"ADM1266",
	"ADM1272",
	"ADM1275",
	"ADM1278",
	"ADM1293",
	"ADS7830",
	"BMR490",
	"DPS800",
	"INA219",
	"INA230",
	"IPSPS",
	"IR38060",
	"IR38164",
	"IR38263",
	"ISL68137",
	"ISL68220",
	"ISL68223",
	"ISL69225",
	"ISL69243",
	"ISL69260",
	"LM25066",
	"MAX16601",
	"MAX20710",
	"MAX20730",
	"MAX20734",
	"MAX20796",
	"MAX34451",
	"MP2971",
	"MP2973",
	"MP5023",
	"PLI1209BC",
	"pmbus",
	"PXE1610",
	"RAA228000",
	"RAA228228",
	"RAA228620",
	"RAA229001",
	"RAA229004",
	"RAA229126",
	"TPS53679",
	"TPS546D24",
	"XDPE11280",
	"XDPE12284"
};

enum HwmonSubType {
	PSU,
	HwmonTemp,
}

impl HwmonSensorConfig {
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, _intfs: &HashMap<String, dbus::arg::PropMap>) -> Option<Self> {
		use dbus::arg::prop_cast;
		let name: &String = prop_cast(basecfg, "Name")?;
		let mut name_overrides: HashMap<String, String> = HashMap::new();
		let r#type = prop_cast::<String>(basecfg, "Type")?.clone();
		let poll_sec: u64 = prop_cast(basecfg, "PollRate").copied().unwrap_or(1);
		let poll_interval = Duration::from_secs(poll_sec);
		let power_state = PowerState::from_dbus(basecfg.get("PowerState"))?;
		let mut names = vec![name.clone()];
		for i in 1.. {
			let key = format!("Name{}", i);
			if let Some(s) = prop_cast::<String>(basecfg, &key) {
				names.push(s.clone());
			} else {
				break;
			}
		}
		let i2c = I2CDeviceParams::from_dbus(basecfg, &r#type).ok()??;
		let enabled_labels: FilterSet<String> = prop_cast(basecfg, "Labels")
			.map(|v: &Vec<_>| HashSet::from_iter(v.iter().cloned()))
			.into();

		for (key, value) in basecfg {
			if let Some(lbl) = key.strip_suffix("_Name") {
				if let Some(s) = value.as_str() {
					name_overrides.insert(lbl.into(), s.into());
				} else {
					eprintln!("{}: {} value not string, ignored", name, key);
				}
			}
		}

		Some(Self {
			names,
			name_overrides,
			i2c,
			poll_interval,
			power_state,
			enabled_labels,
		})
	}

	fn subtype(&self) -> HwmonSubType {
		if PSU_TYPES.contains(&self.i2c.devtype) {
			HwmonSubType::PSU
		} else {
			HwmonSubType::HwmonTemp
		}
	}

	fn sensor_name(&self, idx: usize, label: &str) -> Option<String> {
		// PSU-style configs and hwmon-style configs use
		// different naming schemes
		match self.subtype() {
			HwmonSubType::PSU => {
				let subname: &str = self.name_overrides.get(label)
					.map(|s| s.as_str())
					.unwrap_or(name_for_label(&label));

				Some(format!("{} {}", self.names[0], subname))
			},
			HwmonSubType::HwmonTemp => self.names.get(idx).map(|s| s.into()),
		}
	}
}

fn name_for_label(label: &str) -> &str {
	let tag = label.trim_end_matches(|c: char| c.is_ascii_digit());
	match tag {
		"pin"          => "Input Power",
		"pout"|"power" => "Output Power",
		"maxpin"       => "Max Input Power",
		"vin"          => "Input Voltage",
		"maxvin"       => "Max Input Voltage",
		"vout"|"in"    => "Output Voltage",
		"vmon"         => "Auxiliary Input Voltage",
		"iin"          => "Input Current",
		"iout"|"curr"  => "Output Current",
		"maxiout"      => "Max Output Current",
		"temp"         => "Temperature",
		"maxtemp"      => "Max Temperature",

		// PSUSensorMain.cpp in dbus-sensors uses hard-coded
		// numeric suffixes for these (unlike all the others)
		// -- I'm not sure why or if it matters, but we'll
		// replicate it for now at least...
		"fan" => match label {
			"fan1" => "Fan Speed 1",
			"fan2" => "Fan Speed 2",
			"fan3" => "Fan Speed 3",
			"fan4" => "Fan Speed 4",
			_ => label,
		},

		_ => label,
	}
}

pub async fn update_sensors(cfg: &SensorConfigMap<'_>, sensors: &mut DBusSensorMap,
			    dbuspaths: &FilterSet<dbus::Path<'_>>,
			    i2cdevs: &mut I2CDeviceMap) ->ErrResult<()> {
	let configs = cfg.iter()
		.filter_map(|(path, arccfg)| {
			match arccfg.as_ref() {
				SensorConfig::Hwmon(cfg) if dbuspaths.contains(path) => Some((arccfg, cfg)),
				_ => None,
			}
		});
	for (arccfg, sensorcfg) in configs {
		let mainname = &sensorcfg.names[0];

		if !sensorcfg.power_state.active_now().await {
			// FIXME: log noise
			eprintln!("{}: not active, skipping...", mainname);
			continue;
		}

		let i2cparams = &sensorcfg.i2c;

		let sysfs_dir = i2cparams.sysfs_device_dir();

		let i2cdev = match get_i2cdev(i2cdevs, i2cparams) {
			Ok(d) => d,
			Err(e) => {
				eprintln!("{}: i2c device instantiation failed, skipping: {}", mainname, e);
				continue;
			},
		};

		let hwmondir = match sensor::get_single_hwmon_dir(&sysfs_dir) {
			Ok(Some(d)) => d,
			Ok(None) => {
				eprintln!("{}: no i2c hwmon dir, dynamic?", mainname);
				continue;
			},
			Err(e) => {
				eprintln!("{}: finding i2c hwmon dir: {}", mainname, e);
				continue;
			},
		};

		let pattern = match sensorcfg.subtype() {
			HwmonSubType::PSU => "*_input",
			HwmonSubType::HwmonTemp => "temp*_input",
		};

		let inputs = match glob::glob(&hwmondir.join(pattern).to_string_lossy()) {
			Ok(p) => p,
			Err(e) => {
				eprintln!("{}: error scanning {}, skipping sensor: {}", mainname, hwmondir.display(), e);
				continue;
			},
		};

		// Since we'll need each of them shortly, produce from each matched path:
		struct HwmonFile {
			abspath: std::path::PathBuf,

			// Filename with  "_input" stripped off, e.g. "in1", "temp3", etc.
			// Allocation here is unfortunate, but AFAIK we can't borrow from
			// abspath without messing around with Pin and such.
			base: String,

			kind: SensorType,

			// Just the numeric part of base, parsed out (for sorting)
			idx: usize,
		}

		let mut inputs: Vec<_> = inputs
			.filter_map(|g| {
				match g {
					// wrap this arm in a call to simplify control
					// flow with early returns
					Ok(abspath) => (|| {
						let skip = || {
							eprintln!("{}: don't know how to handle {}, skipping",
								  mainname, abspath.display());
						};

						// .unwrap()s because we know from glob()
						// above that it'll have a filename, and
						// that that filename will end in "_input"
						let base = match abspath.file_name().unwrap().to_str() {
							Some(s) => s.strip_suffix("_input")
								.unwrap()
								.to_string(),
							_ => {
								skip();
								return None;
							},
						};

						let typetag = base.trim_end_matches(|c: char| c.is_ascii_digit());
						let kind = match SensorType::from_hwmon_typetag(typetag) {
							Some(t) => t,
							_ => {
								skip();
								return None;
							},
						};

						// unwrap because we're stripping a prefix
						// that we know is there
						let idx = match base.strip_prefix(typetag).unwrap().parse::<usize>() {
							Ok(n) => n,
							_ => {
								skip();
								return None;
							},
						};

						Some(HwmonFile{
							kind,
							idx,
							base,
							abspath,
						})
					})(),
					Err(e) => {
						eprintln!("{}: error scanning {}, skipping entry: {}",
							  mainname, hwmondir.display(), e);
						None
					},
				}
			}).collect();

		inputs.sort_by_key(|f| (f.kind, f.idx));

		for (idx, file) in inputs.iter().enumerate() {
			// .unwrap() because we know from glob() above that it'll have a
			// final component to strip
			let labelpath = file.abspath.parent().unwrap().join(format!("{}_label", file.base));
			let label = if labelpath.is_file() {
				match std::fs::read_to_string(&labelpath) {
					Ok(s) => s.trim().to_string(),
					Err(e) => {
						eprintln!("{}: error reading {}, skipping entry: {}", mainname, labelpath.display(), e);
						continue;
					},
				}
			} else {
				file.base.to_string()
			};

			if !sensorcfg.enabled_labels.contains(&label) {
				continue;
			}

			let sensorname = match sensorcfg.sensor_name(idx, &label) {
				Some(n) => n,
				_ => {
					eprintln!("{}: {} does not appear to be in use, skipping", mainname, label);
					continue;
				},
			};

			let entry = match sensor::get_nonactive_sensor_entry(sensors, sensorname.clone()).await {
				Some(e) => e,
				None => continue,
			};

			let fd = match std::fs::File::open(&file.abspath) {
				Ok(fd) => fd,
				Err(e) => {
					eprintln!("{}: skipping {}: {}", sensorname, file.abspath.display(), e);
					continue;
				},
			};

			let sensor = Sensor::new(arccfg.clone(), &sensorname, file.kind, fd)
				.with_poll_interval(sensorcfg.poll_interval)
				.with_i2cdev(i2cdev.clone())
				.with_power_state(sensorcfg.power_state);

			// .expect() because we checked for Occupied(Active(_)) earlier
			sensor::install_sensor(entry, sensor).await
				.expect("sensor magically reactivated?");
		}
	}

	Ok(())
}
