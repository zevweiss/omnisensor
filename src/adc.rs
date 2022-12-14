//! Backend providing support for ADC sensors.
//!
//! A la dbus-sensors's `adcsensor` daemon.

use std::{
	collections::HashMap,
	sync::Arc,
	time::Duration,
};

use crate::{
	DaemonState,
	types::*,
	gpio::BridgeGPIOConfig,
	powerstate::PowerState,
	sensor,
	sensor::{
		Sensor,
		SensorConfig,
		SensorType,
	},
	sysfs,
	threshold,
	threshold::ThresholdConfig,
	dbus_helpers::props::*,
};

/// Internal representation of the dbus config data for an ADC sensor (one channel).
#[derive(Debug)]
pub struct ADCSensorConfig {
	/// Sensor name.
	name: String,
	/// Index of this sensor (channel) within the ADC hardware device.
	index: u64,
	/// Polling interval for the sensor.
	poll_interval: Duration,
	/// Scaling multiplier for the sensor.
	///
	/// We store this as the reciprocal of what was provided via the dbus config so
	/// that we can multiply instead of dividing
	scale: f64,
	/// Host power state in which this sensor is active.
	power_state: PowerState,
	/// Threshold settings for the sensor.
	thresholds: Vec<ThresholdConfig>,
	/// An optional GPIO that must be asserted before reading the sensor.
	///
	/// Common for battery voltage sensors to reduce parasitic battery drain.
	bridge_gpio: Option<Arc<BridgeGPIOConfig>>,
}

impl ADCSensorConfig {
	/// Construct an [`ADCSensorConfig`] from raw dbus config data.
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str, intfs: &HashMap<String, dbus::arg::PropMap>) -> ErrResult<Self> {
		let name: &String = prop_get_mandatory(basecfg, "Name")?;
		let index: u64 = *prop_get_mandatory(basecfg, "Index")?;
		let poll_sec: u64 = *prop_get_default(basecfg, "PollRate", &1u64)?;
		let scale: f64 = *prop_get_default(basecfg, "ScaleFactor", &1.0f64)?;
		let power_state = prop_get_default_from::<str, _>(basecfg, "PowerState", PowerState::Always)?;
		let bridge_gpio = match intfs.get("xyz.openbmc_project.Configuration.ADC.BridgeGpio0") {
			Some(map) => Some(Arc::new(BridgeGPIOConfig::from_dbus(map)?)),
			None => None,
		};
		let thresholds = threshold::get_configs_from_dbus(baseintf, intfs);

		if !scale.is_finite() || scale == 0.0 {
			let msg = format!("{}: ScaleFactor must be finite and non-zero (got {})", name, scale);
			return Err(err_invalid_data(msg));
		}

		Ok(Self {
			name: name.clone(),
			index,
			poll_interval: Duration::from_secs(poll_sec),
			scale: 1.0 / scale, // convert to a multiplier
			power_state,
			thresholds,
			bridge_gpio,
		})
	}
}

/// The directory where we expect to find the ADC sensor device.
const IIO_HWMON_PATH: &str = "/sys/devices/platform/iio-hwmon";

/// Instantiate any active ADC sensors configured in `cfgmap`.
pub async fn instantiate_sensors(daemonstate: &DaemonState, dbuspaths: &FilterSet<InventoryPath>) -> ErrResult<()> {
	let hwmondir = sysfs::get_single_hwmon_dir(std::path::Path::new(IIO_HWMON_PATH))?;
	let cfgmap = daemonstate.config.lock().await;
	let configs = cfgmap.iter()
		.filter_map(|(path, cfg)| {
			match cfg {
				SensorConfig::ADC(adccfg) if dbuspaths.contains(path) => Some(adccfg),
				_ => None,
			}
		});
	for adccfg in configs {
		let ioctx = match sysfs::prepare_indexed_hwmon_ioctx(&hwmondir, adccfg.index, SensorType::Voltage,
								     adccfg.power_state, &adccfg.bridge_gpio).await {
			Ok(Some(ioctx)) => ioctx,
			Ok(None) => continue,
			Err(e) => {
				eprintln!("Error preparing {} from {}: {}", adccfg.name, hwmondir.display(), e);
				continue;
			},
		};

		let mut sensors = daemonstate.sensors.lock().await;

		let Some(entry) = sensor::get_nonactive_sensor_entry(&mut sensors, adccfg.name.clone()).await else {
			continue;
		};

		sensor::install_or_activate(entry, &daemonstate.crossroads, ioctx, &daemonstate.sensor_intfs, || {
			Sensor::new(&adccfg.name, SensorType::Voltage, &daemonstate.sensor_intfs, &daemonstate.bus)
				.with_poll_interval(adccfg.poll_interval)
				.with_scale(adccfg.scale)
				.with_power_state(adccfg.power_state)
				.with_thresholds_from(&adccfg.thresholds, &daemonstate.sensor_intfs.thresholds, &daemonstate.bus)
				.with_minval(0.0)
				.with_maxval(1.8 * adccfg.scale) // 1.8 cargo-culted from ADCSensorMain.cpp
		}).await;
	}
	Ok(())
}
