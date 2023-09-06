//! Backend providing support for fan sensors.
//!
//! A la dbus-sensors's `fansensor` daemon.

use std::{
	collections::{HashMap, HashSet},
	path::PathBuf,
};
use log::error;

use crate::{
	DaemonState,
	dbus_helpers::props::*,
	devices::i2c,
	powerstate::PowerState,
	sensor,
	sensor::{
		Sensor,
		SensorConfig,
		SensorMode::ReadOnly,
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

	/// I2C-attached fan controller.
	I2CFan(i2c::I2CLocation),
}

impl FanSensorType {
	/// Return a function that can be called on a map of dbus config properties to
	/// construct a [`FanSensorType`] instance for the given type.
	fn get_ctor(s: &str) -> ErrResult<fn(&dbus::arg::PropMap) -> ErrResult<Self>> {
		fn aspeed_ctor(_: &dbus::arg::PropMap) -> ErrResult<FanSensorType> {
			Ok(FanSensorType::AspeedFan)
		}
		fn i2c_ctor(cfg: &dbus::arg::PropMap) -> ErrResult<FanSensorType> {
			Ok(FanSensorType::I2CFan(i2c::I2CLocation::from_dbus(cfg)?))
		}
		match s {
			"AspeedFan" => Ok(aspeed_ctor),
			"I2CFan" => Ok(i2c_ctor),
			_ => Err(err_unsupported(format!("Unsupported fan sensor type '{}'", s))),
		}
	}
}

/// Fan sensor config parameters common to all sub-types of fan sensors.
#[derive(Debug)]
struct FanSensorBaseConfig {
	/// Index of this particular sensor (channel) within the containing hardware device.
	index: u64,
	/// Sensor name.
	name: String,
	/// Host power state in which this sensor is active.
	power_state: PowerState,
	/// Threshold settings for the sensor.
	thresholds: Vec<threshold::ThresholdConfig>,
	/// Minimum reading value for the sensor.
	minreading: f64,
	/// Maximum reading value for the sensor.
	maxreading: f64,
}

impl FanSensorBaseConfig {
	/// Construct a [`FanSensorBaseConfig`] from raw dbus config data.
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str,
	                 intfs: &HashMap<String, dbus::arg::PropMap>) -> ErrResult<Self> {
		let index = *prop_get_mandatory(basecfg, "Index")?;
		let name: &String = prop_get_mandatory(basecfg, "Name")?;
		let power_state = prop_get_default_from(basecfg, "PowerState", PowerState::Always)?;
		let thresholds = threshold::get_configs_from_dbus(baseintf, intfs);
		let minreading = *prop_get_default(basecfg, "MinReading", &0.0f64)?;

		// default carried over from dbus-sensors's fansensor
		let maxreading = *prop_get_default(basecfg, "MaxReading", &25000.0f64)?;

		Ok(Self {
			index,
			name: name.clone(),
			power_state,
			thresholds,
			minreading,
			maxreading,
		})
	}
}

/// Internal representation of fan sensor config data from dbus.
#[derive(Debug)]
pub struct FanSensorConfig {
	/// Sub-type of fan sensor.
	subtype: FanSensorType,
	/// Parameters common to all sub-types.
	common: FanSensorBaseConfig,
}

impl FanSensorConfig {
	/// Construct a [`FanSensorConfig`] from raw dbus config data.
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str,
	                 intfs: &HashMap<String, dbus::arg::PropMap>) -> ErrResult<Self> {
		let common = FanSensorBaseConfig::from_dbus(basecfg, baseintf, intfs)?;
		let subtype_str: &String = prop_get_mandatory(basecfg, "Type")?;
		let subtype_ctor = FanSensorType::get_ctor(subtype_str)?;
		let subtype = subtype_ctor(basecfg)?;

		Ok(Self {
			subtype,
			common,
		})
	}

	/// Return the path of a fan sensor's hwmon directory.
	fn hwmon_dir(&self) -> ErrResult<PathBuf> {
		let devdir = match &self.subtype {
			FanSensorType::AspeedFan => {
				let pattern = format!("{}/*.pwm-tacho-controller", sysfs::PLATFORM_DEVICE_DIR);
				sysfs::get_single_glob_match(&pattern)?
			},
			FanSensorType::I2CFan(loc) =>  {
				loc.sysfs_device_dir()
			},
		};
		sysfs::get_single_hwmon_dir(&devdir)
	}
}

/// Instantiate any active fan sensors configured in `daemonstate.config`.
pub async fn instantiate_sensors(daemonstate: &DaemonState, dbuspaths: &FilterSet<InventoryPath>,
                                 _retry: &mut HashSet<InventoryPath>) -> ErrResult<()>
{
	let cfgmap = daemonstate.config.lock().await;
	let configs = cfgmap.iter()
		.filter_map(|(path, cfg)| {
			match cfg {
				SensorConfig::Fan(c) if dbuspaths.contains(path) => Some((path, c)),
				_ => None,
			}
		});
	for (path, fancfg) in configs {
		let mut sensors = daemonstate.sensors.lock().await;
		let hwmondir = fancfg.hwmon_dir()?;
		let fancfg = &fancfg.common;

		let Some(entry) = sensor::get_nonactive_sensor_entry(&mut sensors,
		                                                     fancfg.name.clone()).await else {
			continue;
		};

		let ioctx = match sysfs::prepare_indexed_hwmon_ioctx(&hwmondir, fancfg.index,
		                                                     SensorType::RPM,
		                                                     fancfg.power_state, &None).await {
			Ok(Some(ioctx)) => ioctx,
			Ok(None) => continue,
			Err(e) => {
				error!("Error preparing {} from {}: {}", fancfg.name,
				       hwmondir.display(), e);
				continue;
			},
		};

		let ctor = || {
			Sensor::new(path,&fancfg.name, SensorType::RPM, &daemonstate.sensor_intfs,
			            &daemonstate.bus, ReadOnly)
				.with_power_state(fancfg.power_state)
				.with_thresholds_from(&fancfg.thresholds,
				                      &daemonstate.sensor_intfs.thresholds,
				                      &daemonstate.bus)
				.with_minval(fancfg.minreading)
				.with_maxval(fancfg.maxreading)
		};
		sensor::install_or_activate(entry, &daemonstate.crossroads, ioctx,
		                            &daemonstate.sensor_intfs, ctor).await;
	}
	Ok(())
}

/// Whether or not the given `cfgtype` is supported by the `fan` sensor backend.
pub fn match_cfgtype(cfgtype: &str) -> bool {
	FanSensorType::get_ctor(cfgtype).is_ok()
}
