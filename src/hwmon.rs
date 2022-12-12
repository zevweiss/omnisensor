use std::{
	collections::{HashMap, HashSet},
	sync::{Arc, Mutex as SyncMutex},
	time::Duration,
};
use dbus::{
	arg::RefArg,
	nonblock::SyncConnection,
};
use phf::phf_set;

use crate::{
	types::*,
	i2c::{
		I2CDeviceParams,
		I2CDeviceMap,
		get_i2cdev,
	},
	powerstate::PowerState,
	sensor,
	sensor::{
		Sensor,
		SensorConfig,
		SensorConfigMap,
		SensorIntfData,
		SensorIO,
		SensorMap,
		SensorType,
	},
	sysfs,
	threshold,
	threshold::ThresholdConfig
};

#[derive(Debug)]
pub struct HwmonSensorConfig {
	names: Vec<String>,
	name_overrides: HashMap<String, String>,
	i2c: I2CDeviceParams,
	poll_interval: Duration,
	power_state: PowerState,
	thresholds: Vec<ThresholdConfig>,
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
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str, intfs: &HashMap<String, dbus::arg::PropMap>) -> Option<Self> {
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
		let thresholds = threshold::get_configs_from_dbus(baseintf, intfs);

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
			thresholds,
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
					.unwrap_or_else(|| name_for_label(label));

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

pub async fn update_sensors(cfg: &SensorConfigMap, sensors: &mut SensorMap,
			    dbuspaths: &FilterSet<InventoryPath>, i2cdevs: &mut I2CDeviceMap,
			    cr: &SyncMutex<dbus_crossroads::Crossroads>, sensor_intfs: &SensorIntfData,
			    conn: &Arc<SyncConnection>) ->ErrResult<()> {
	let configs = cfg.iter()
		.filter_map(|(path, cfg)| {
			match cfg {
				SensorConfig::Hwmon(hwmcfg) if dbuspaths.contains(path) => Some(hwmcfg),
				_ => None,
			}
		});
	for hwmcfg in configs {
		let mainname = &hwmcfg.names[0];

		if !hwmcfg.power_state.active_now().await {
			// FIXME: log noise
			eprintln!("{}: not active, skipping...", mainname);
			continue;
		}

		let i2cdev = match get_i2cdev(i2cdevs, &hwmcfg.i2c) {
			Ok(d) => d,
			Err(e) => {
				eprintln!("{}: i2c device instantiation failed, skipping: {}", mainname, e);
				continue;
			},
		};

		let prefix = match hwmcfg.subtype() {
			HwmonSubType::PSU => None,
			HwmonSubType::HwmonTemp => Some("temp"),
		};

		let sysfs_dir = hwmcfg.i2c.sysfs_device_dir();
		let inputs = match sysfs::scan_hwmon_input_files(std::path::Path::new(&sysfs_dir), prefix) {
			Ok(v) => v,
			Err(e) => {
				eprintln!("{}: error scanning {}, skipping sensor: {}", mainname, sysfs_dir, e);
				continue;
			},
		};

		for (idx, file) in inputs.iter().enumerate() {
			let label = match file.get_label() {
				Ok(s) => s,
				Err(e) => {
					eprintln!("{}: error finding label for {}, skipping entry: {}", mainname,
						  file.abspath.display(), e);
					continue;
				},
			};

			if !hwmcfg.enabled_labels.contains(&label) {
				continue;
			}

			let Some(sensorname) = hwmcfg.sensor_name(idx, &label) else {
				eprintln!("{}: {} does not appear to be in use, skipping", mainname, label);
				continue;
			};

			let Some(entry) = sensor::get_nonactive_sensor_entry(sensors, sensorname.clone()).await else {
				continue;
			};

			let fd = match std::fs::File::open(&file.abspath) {
				Ok(fd) => fd,
				Err(e) => {
					eprintln!("{}: skipping {}: {}", sensorname, file.abspath.display(), e);
					continue;
				},
			};

			let (minval, maxval) = match file.kind {
				SensorType::Temperature => (-128.0, 127.0),
				SensorType::RPM => (0.0, 30000.0),
				SensorType::Voltage => (0.0, 255.0),
				SensorType::Current => (0.0, 255.0), // FIXME: PSUSensorMain.cpp has 20 as max for input currents
				SensorType::Power => (0.0, 3000.0),
			};

			let io = SensorIO::new(fd).with_i2cdev(i2cdev.clone());

			sensor::install_or_activate(entry, cr, io, sensor_intfs, || {
				Sensor::new(&sensorname, file.kind, sensor_intfs, conn)
					.with_poll_interval(hwmcfg.poll_interval)
					.with_power_state(hwmcfg.power_state)
					.with_thresholds_from(&hwmcfg.thresholds, &sensor_intfs.thresholds, conn)
					.with_minval(minval)
					.with_maxval(maxval)
			}).await;
		}
	}

	Ok(())
}
