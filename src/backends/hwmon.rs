//! Backend providing support for PMBus and I2C-based hwmon sensors.
//!
//! Combines the functionality of dbus-sensors's `psusensor` and
//! `hwmontempsensor` daemons.

use std::{
	collections::{HashMap, HashSet},
	ops::Deref,
	time::Duration,
};
use dbus::arg::RefArg;
use log::{debug, error};

use crate::{
	DaemonState,
	types::*,
	devices::i2c::{
		I2CDeviceParams,
		I2CDeviceType,
		get_device_type,
		get_i2cdev,
	},
	powerstate::PowerState,
	sensor,
	sensor::{
		Sensor,
		SensorConfig,
		SensorIOCtx,
		SensorMode::ReadOnly,
		SensorType,
	},
	sysfs,
	threshold,
	threshold::ThresholdConfig,
	dbus_helpers::props::*,
};

/// Internal representation of dbus config data for an I2C/PMBus hwmon sensor.
#[derive(Debug)]
pub struct HwmonSensorConfig {
	/// The sensor names used for the channels of this sensor.
	names: Vec<String>,
	/// Alternate names provided by `"<foo>_Name"` config keys.
	name_overrides: HashMap<String, String>,
	/// I2C parameters for the sensor.
	i2c: I2CDeviceParams,
	/// Polling interval for the sensor.
	poll_interval: Duration,
	/// Host power state in which this sensor is active.
	power_state: PowerState,
	/// Threshold settings for the sensor.
	thresholds: Vec<ThresholdConfig>,
	/// Which channel labels are enabled (i.e. should have a sensor created for them).
	enabled_labels: FilterSet<String>,
}

impl HwmonSensorConfig {
	/// Construct a [`HwmonSensorConfig`] from raw dbus config data.
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str,
	                 intfs: &HashMap<String, dbus::arg::PropMap>) -> ErrResult<Self> {
		let name: &String = prop_get_mandatory(basecfg, "Name")?;
		let mut name_overrides: HashMap<String, String> = HashMap::new();
		let r#type = prop_get_mandatory::<String>(basecfg, "Type")?.clone();
		let poll_sec: f64 = prop_get_default_num(basecfg, "PollRate", 1.0)?;
		let poll_interval = Duration::from_secs_f64(poll_sec);
		let power_state = prop_get_default_from(basecfg, "PowerState", PowerState::Always)?;
		let mut names = vec![name.clone()];
		for i in 1.. {
			let key = format!("Name{}", i);
			if let Some(s) = prop_get_optional::<String>(basecfg, &key)? {
				names.push(s.clone());
			} else {
				break;
			}
		}
		let Some(devtype) = get_device_type(&r#type) else {
			return Err(err_unsupported(format!("unsupported device type '{}'", r#type)));
		};
		let i2c = I2CDeviceParams::from_dbus(basecfg, devtype)?;
		let enabled_labels: FilterSet<String> = prop_get_optional(basecfg, "Labels")?
			.map(|v: &Vec<_>| HashSet::from_iter(v.iter().cloned()))
			.into();
		let thresholds = threshold::get_configs_from_dbus(baseintf, intfs);

		for (key, value) in basecfg {
			if let Some(lbl) = key.strip_suffix("_Name") {
				if let Some(s) = value.as_str() {
					name_overrides.insert(lbl.into(), s.into());
				} else {
					error!("{}: {} value not string, ignored", name, key);
				}
			}
		}

		Ok(Self {
			names,
			name_overrides,
			i2c,
			poll_interval,
			power_state,
			thresholds,
			enabled_labels,
		})
	}

	/// Determine the sensor name to use for a given `idx` and `label`.
	fn sensor_name(&self, idx: usize, label: &str) -> Option<String> {
		// PSU-style configs and hwmon-style configs use
		// different naming schemes
		match self.i2c.devtype {
			I2CDeviceType::PMBus(_) => {
				let subname: &str = self.name_overrides.get(label)
					.map(|s| s.as_str())
					.unwrap_or_else(|| name_for_label(label));

				Some(format!("{} {}", self.names[0], subname))
			},
			I2CDeviceType::Hwmon(_) => self.names.get(idx).map(|s| s.into()),
		}
	}
}

/// Determine the sensor name (component) corresponding to a given hwmon label tag.
fn name_for_label(label: &str) -> &str {
	let tag = label.trim_end_matches(|c: char| c.is_ascii_digit());
	match tag {
		"pin"               => "Input Power",
		"pout"|"power"      => "Output Power",
		"maxpin"            => "Max Input Power",
		"vin"               => "Input Voltage",
		"maxvin"            => "Max Input Voltage",
		"vout"|"in"         => "Output Voltage",
		"vmon"              => "Auxiliary Input Voltage",
		"iin"               => "Input Current",
		"iout"|"curr"       => "Output Current",
		"maxiout"           => "Max Output Current",
		"temp"              => "Temperature",
		"maxtemp"           => "Max Temperature",

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

/// Instantiate any active PMBus/I2C hwmon sensors configured in `daemonstate.config`.
pub async fn instantiate_sensors(daemonstate: &DaemonState, dbuspaths: &FilterSet<InventoryPath>,
                                 retry: &mut HashSet<InventoryPath>) -> ErrResult<()>
{
	let cfgmap = daemonstate.config.lock().await;
	let configs = cfgmap.iter()
		.filter_map(|(path, cfg)| {
			match cfg {
				SensorConfig::Hwmon(c) if dbuspaths.contains(path) => Some((path, c)),
				_ => None,
			}
		});
	for (path, hwmcfg) in configs {
		let mainname = &hwmcfg.names[0];

		if !hwmcfg.power_state.active_now() {
			debug!("{}: not active, skipping...", mainname);
			continue;
		}

		let physdev = {
			let mut physdevs = daemonstate.physdevs.lock().await;
			match get_i2cdev(&mut physdevs, &hwmcfg.i2c) {
				Ok(d) => d,
				Err(e) => {
					error!("{}: i2c device instantiation failed: {}",
					       mainname, e);
					retry.insert(path.deref().clone());
					continue;
				},
			}
		};

		let prefix = match hwmcfg.i2c.devtype {
			I2CDeviceType::PMBus(_) => None,
			I2CDeviceType::Hwmon(_) => Some("temp"),
		};

		let sysfs_dir = hwmcfg.i2c.sysfs_device_dir();
		let inputs = match sysfs::scan_hwmon_input_files(&sysfs_dir, prefix) {
			Ok(v) => v,
			Err(e) => {
				error!("{}: error scanning {}, skipping sensor: {}", mainname,
				       sysfs_dir.display(), e);
				continue;
			},
		};

		for (idx, file) in inputs.iter().enumerate() {
			let label = match file.get_label() {
				Ok(s) => s,
				Err(e) => {
					error!("{}: error finding label for {}, skipping entry: {}",
					       mainname, file.abspath.display(), e);
					continue;
				},
			};

			if !hwmcfg.enabled_labels.contains(&label) {
				continue;
			}

			let Some(sensorname) = hwmcfg.sensor_name(idx, &label) else {
				debug!("{}: {} does not appear to be in use, skipping",
				       mainname, label);
				continue;
			};

			let mut sensors = daemonstate.sensors.lock().await;

			let Some(entry) = sensor::get_nonactive_sensor_entry(&mut sensors,
			                                                     sensorname.clone()).await else {
				continue;
			};

			let io = match sysfs::SysfsSensorIO::new(file).await {
				Ok(io) => sensor::SensorIO::Sysfs(io),
				Err(e) => {
					error!("{}: skipping {}: {}", sensorname,
					       file.abspath.display(), e);
					continue;
				},
			};

			let (minval, maxval) = match file.kind {
				SensorType::Temperature => (-128.0, 127.0),
				SensorType::RPM => (0.0, 30000.0),
				SensorType::Voltage => (0.0, 255.0),
				// FIXME: PSUSensorMain.cpp has 20 as max for input currents
				SensorType::Current => (0.0, 255.0),
				SensorType::Power => (0.0, 3000.0),
			};

			let io = SensorIOCtx::new(io).with_physdev(physdev.clone());

			let ctor = || {
				Sensor::new(path, &sensorname, file.kind, &daemonstate.sensor_intfs,
				            &daemonstate.bus, ReadOnly)
					.with_poll_interval(hwmcfg.poll_interval)
					.with_power_state(hwmcfg.power_state)
					.with_thresholds_from(&hwmcfg.thresholds, Some(idx + 1),
					                      &daemonstate.sensor_intfs.thresholds,
					                      &daemonstate.bus)
					.with_minval(minval)
					.with_maxval(maxval)
			};
			sensor::install_or_activate(entry, &daemonstate.crossroads, io,
			                            &daemonstate.sensor_intfs, ctor).await;
		}
	}

	Ok(())
}

/// Whether or not the given `cfgtype` is supported by the `hwmon` sensor backend.
pub fn match_cfgtype(cfgtype: &str) -> bool {
	get_device_type(cfgtype).is_some()
}
