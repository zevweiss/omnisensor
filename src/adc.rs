use std::{
	collections::HashMap,
	sync::Arc,
	time::Duration,
};
use dbus::nonblock::SyncConnection;

use crate::{
	types::*,
	gpio::{BridgeGPIOConfig, BridgeGPIO},
	powerstate::PowerState,
	sensor,
	sensor::{
		DBusSensorMap,
		Sensor,
		SensorConfig,
		SensorConfigMap,
		SensorIntfData,
		SensorType,
	},
	threshold,
	threshold::ThresholdConfig,
};

#[derive(Debug)]
pub struct ADCSensorConfig {
	name: String,
	index: u64,
	poll_interval: Duration,
	// We store this as the reciprocal of what was found in the
	// config, so that we can multiply instead of dividing
	scale: f64,
	power_state: PowerState,
	thresholds: Vec<ThresholdConfig>,
	bridge_gpio: Option<Arc<BridgeGPIOConfig>>,
}

impl ADCSensorConfig {
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str, intfs: &HashMap<String, dbus::arg::PropMap>) -> Option<Self> {
		use dbus::arg::prop_cast;
		let name: &String = prop_cast(basecfg, "Name")?;
		let index: u64 = *prop_cast(basecfg, "Index")?;
		let poll_sec: u64 = prop_cast(basecfg, "PollRate").copied().unwrap_or(1);
		let scale: f64 = prop_cast(basecfg, "ScaleFactor").copied().unwrap_or(1.0);
		let power_state = PowerState::from_dbus(basecfg.get("PowerState"))?;
		let bridge_gpio = intfs.get("xyz.openbmc_project.Configuration.ADC.BridgeGpio0")
			.and_then(BridgeGPIOConfig::from_dbus).map(Arc::new);
		let thresholds = threshold::get_configs_from_dbus(baseintf, intfs);

		if !scale.is_finite() || scale == 0.0 {
			eprintln!("{}: ScaleFactor must be finite and non-zero (got {})", name, scale);
			return None
		}

		Some(Self {
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

const IIO_HWMON_PATH: &str = "/sys/devices/platform/iio-hwmon";

// Returns a Vec of ... ordered by index (inX_input, E-M config
// "Index" key)
fn find_adc_sensors() -> ErrResult<Vec<std::path::PathBuf>> {
	let Some(devdir) = sensor::get_single_hwmon_dir(IIO_HWMON_PATH)? else {
		return Ok(vec![]);
	};

	let mut paths: Vec<_> = vec![];
	for i in 1.. {
		let p = devdir.join(format!("in{}_input", i));
		if !p.is_file() {
			break;
		}
		paths.push(p);
	}
	Ok(paths)
}

pub async fn update_sensors(cfgmap: &SensorConfigMap, sensors: &mut DBusSensorMap,
			    dbuspaths: &FilterSet<dbus::Path<'_>>, conn: &Arc<SyncConnection>,
			    sensor_intfs: &SensorIntfData) -> ErrResult<()> {
	let adcpaths = find_adc_sensors()?; // FIXME (error handling)
	let configs = cfgmap.iter()
		.filter_map(|(path, cfg)| {
			match cfg {
				SensorConfig::ADC(adccfg) if dbuspaths.contains(path) => Some((path, adccfg)),
				_ => None,
			}
		});
	for (dbuspath, adccfg) in configs {
		if adccfg.index >= adcpaths.len() as u64 {
			eprintln!("{} ignored, no corresponding file found",
				  adccfg.name);
			continue;
		}

		if !adccfg.power_state.active_now().await {
			// FIXME: log noise
			eprintln!("{}: not active, skipping...", adccfg.name);
			continue;
		}

		let Some(entry) = sensor::get_nonactive_sensor_entry(sensors, adccfg.name.clone()).await else {
			continue;
		};

		let path = &adcpaths[adccfg.index as usize];
		let bridge_gpio = match &adccfg.bridge_gpio {
			Some(c) => match BridgeGPIO::from_config(c.clone()) {
				Ok(c) => Some(c),
				Err(e) => {
					eprintln!("Failed to get bridge GPIO {} for {}: {}", c.name,
						  adccfg.name, e);
					continue;
				}
			},
			None => None,
		};

		let fd = match std::fs::File::open(path) {
			Ok(f) => f,
			Err(e) => {
				eprintln!("Failed to open {} for {}: {}",
					  path.display(), adccfg.name, e);
				continue;
			},
		};

		let thresholds = threshold::get_thresholds_from_configs(&adccfg.thresholds,
									&sensor_intfs.thresholds, dbuspath, conn);

		let sensor = Sensor::new(&adccfg.name, SensorType::Voltage, fd, sensor_intfs, dbuspath, conn)
			.with_poll_interval(adccfg.poll_interval)
			.with_scale(adccfg.scale)
			.with_bridge_gpio(bridge_gpio)
			.with_power_state(adccfg.power_state)
			.with_thresholds(thresholds);

		// .expect() because we checked for Occupied(Active(_)) earlier
		sensor::install_sensor(entry, sensor).await
			.expect("sensor magically reactivated?");
	}
	Ok(())
}
