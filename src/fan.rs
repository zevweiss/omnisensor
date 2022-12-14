//! Backend providing support for fan sensors.
//!
//! A la dbus-sensors's `fansensor` daemon.

use std::collections::HashMap;

use crate::{
	DaemonState,
	dbus_helpers::props::*,
	powerstate::PowerState,
	sensor,
	sensor::{
		Sensor,
		SensorConfig,
		SensorIOCtx,
		SensorType,
	},
	sysfs,
	threshold,
	types::*,
};

/// An enum representing which specific variety of fan sensor we're dealing with.
#[derive(Debug)]
enum FanSensorType {
	/// ASPEED AST2x00 fan tach/pwm.
	AspeedFan,
}

impl TryFrom<&str> for FanSensorType {
	type Error = Box<dyn std::error::Error>;
	fn try_from(s: &str) -> ErrResult<Self> {
		match s {
			"AspeedFan" => Ok(Self::AspeedFan),
			_ => Err(err_unsupported(format!("Unsupported fan sensor type '{}'", s))),
		}
	}
}

/// Internal representation of fan sensor config data from dbus.
#[derive(Debug)]
pub struct FanSensorConfig {
	/// Index of this particular sensor (channel) within the containing hardware device.
	index: u64,
	/// Sensor name.
	name: String,
	/// Host power state in which this sensor is active.
	power_state: PowerState,
	/// Sub-type of fan sensor.
	subtype: FanSensorType,
	/// Threshold settings for the sensor.
	thresholds: Vec<threshold::ThresholdConfig>,
	/// Minimum reading value for the sensor.
	minreading: f64,
	/// Maximum reading value for the sensor.
	maxreading: f64,
}

impl FanSensorConfig {
	/// Construct a [`FanSensorConfig`] from raw dbus config data.
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str, intfs: &HashMap<String, dbus::arg::PropMap>) -> ErrResult<Self> {
		let index = *prop_get_mandatory(basecfg, "Index")?;
		let name: &String = prop_get_mandatory(basecfg, "Name")?;
		let power_state = prop_get_default_from::<str, _>(basecfg, "PowerState", PowerState::Always)?;
		let subtype = prop_get_mandatory_from::<str, FanSensorType>(basecfg, "Type")?;
		let thresholds = threshold::get_configs_from_dbus(baseintf, intfs);
		let minreading = *prop_get_default(basecfg, "MinReading", &0.0f64)?;
		let maxreading = *prop_get_default(basecfg, "MaxReading", &25000.0f64)?; // default carried over from dbus-sensors's fansensor
		Ok(Self {
			index,
			name: name.clone(),
			power_state,
			subtype,
			thresholds,
			minreading,
			maxreading,
		})
	}
}

/// Instantiate any active fan sensors configured in `cfgmap`.
pub async fn instantiate_sensors(daemonstate: &DaemonState, dbuspaths: &FilterSet<InventoryPath>) ->ErrResult<()> {
	let cfgmap = daemonstate.config.lock().await;
	let configs = cfgmap.iter()
		.filter_map(|(path, cfg)| {
			match cfg {
				SensorConfig::Fan(fancfg) if dbuspaths.contains(path) => Some(fancfg),
				_ => None,
			}
		});
	let pattern = format!("{}/*.pwm-tacho-controller", sysfs::PLATFORM_DEVICE_DIR);
	let controller_dir = sysfs::get_single_glob_match(&pattern)?;
	let hwmondir = sysfs::get_single_hwmon_dir(&controller_dir)?;
	for fancfg in configs {
		if !fancfg.power_state.active_now() {
			continue;
		}

		let path = hwmondir.join(format!("fan{}_input", fancfg.index + 1));
		let file = match sysfs::HwmonFileInfo::from_abspath(path) {
			Ok(f) => f,
			Err(e) => {
				eprintln!("{}: Error getting input file for index {}: {}", fancfg.name,
					  fancfg.index, e);
				continue;
			},
		};

		let mut sensors = daemonstate.sensors.lock().await;

		let Some(entry) = sensor::get_nonactive_sensor_entry(&mut sensors, fancfg.name.clone()).await else {
			continue;
		};

		let io = match sysfs::SysfsSensorIO::new(&file).await {
			Ok(io) => io,
			Err(e) => {
				eprintln!("Failed to open {} for {}: {}", file.abspath.display(),
					  fancfg.name, e);
				continue;
			},
		};

		let io = SensorIOCtx::new(io);
		sensor::install_or_activate(entry, &daemonstate.crossroads, io, &daemonstate.sensor_intfs, || {
			Sensor::new(&fancfg.name, SensorType::RPM, &daemonstate.sensor_intfs, &daemonstate.bus)
				.with_power_state(fancfg.power_state)
				.with_thresholds_from(&fancfg.thresholds, &daemonstate.sensor_intfs.thresholds, &daemonstate.bus)
				.with_minval(fancfg.minreading)
				.with_maxval(fancfg.maxreading)
		}).await;
	}
	Ok(())
}
