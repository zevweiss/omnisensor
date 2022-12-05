use std::{
	collections::HashMap,
	sync::Arc,
	time::Duration,
};
use dbus;

use crate::{
	types::{ErrResult, FilterSet},
	gpio::{BridgeGPIOConfig, BridgeGPIO},
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
pub struct ADCSensorConfig {
	name: String,
	index: u64,
	poll_interval: Duration,
	// We store this as the reciprocal of what was found in the
	// config, so that we can multiply instead of dividing
	scale: f64,
	power_state: PowerState,
	bridge_gpio: Option<Arc<BridgeGPIOConfig>>,
}

impl ADCSensorConfig {
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, intfs: &HashMap<String, dbus::arg::PropMap>) -> Option<Self> {
		use dbus::arg::prop_cast;
		let name: &String = prop_cast(basecfg, "Name")?;
		let index: u64 = *prop_cast(basecfg, "Index")?;
		let poll_sec: u64 = prop_cast(basecfg, "PollRate").copied().unwrap_or(1);
		let scale: f64 = prop_cast(basecfg, "ScaleFactor").copied().unwrap_or(1.0);
		let power_state = PowerState::from_dbus(basecfg.get("PowerState"))?;
		let bridge_gpio = intfs.get("xyz.openbmc_project.Configuration.ADC.BridgeGpio0")
			.map(BridgeGPIOConfig::from_dbus).flatten().map(Arc::new);

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
			bridge_gpio,
		})
	}
}

const IIO_HWMON_PATH: &'static str = "/sys/devices/platform/iio-hwmon";

// Returns a Vec of ... ordered by index (inX_input, E-M config
// "Index" key)
fn find_adc_sensors() -> ErrResult<Vec<std::path::PathBuf>> {
	let devdir = match sensor::get_single_hwmon_dir(IIO_HWMON_PATH)? {
		Some(d) => d,
		None => return Ok(vec![]),
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

pub async fn update_sensors(cfgmap: &SensorConfigMap<'_>, sensors: &mut DBusSensorMap, dbuspaths: &FilterSet<dbus::Path<'_>>) ->ErrResult<()> {
	let adcpaths = find_adc_sensors()?; // FIXME (error handling)
	let configs = cfgmap.iter()
		.filter_map(|(path, arccfg)| {
			match arccfg.as_ref() {
				SensorConfig::ADC(cfg) if dbuspaths.contains(path) => Some((path, arccfg, cfg)),
				_ => None,
			}
		});
	for (dbuspath, arccfg, sensorcfg) in configs {
		if sensorcfg.index >= adcpaths.len() as u64 {
			eprintln!("{} ignored, no corresponding file found",
				  sensorcfg.name);
			continue;
		}

		if !sensorcfg.power_state.active_now().await {
			// FIXME: log noise
			eprintln!("{}: not active, skipping...", sensorcfg.name);
			continue;
		}

		let entry = match sensor::get_nonactive_sensor_entry(sensors, sensorcfg.name.clone()).await {
			Some(e) => e,
			None => continue,
		};

		let path = &adcpaths[sensorcfg.index as usize];
		let bridge_gpio = match &sensorcfg.bridge_gpio {
			Some(c) => match BridgeGPIO::from_config(c.clone()) {
				Ok(c) => Some(c),
				Err(e) => {
					eprintln!("Failed to get bridge GPIO {} for {}: {}", c.name,
						  sensorcfg.name, e);
					continue;
				}
			},
			None => None,
		};

		let fd = match std::fs::File::open(&path) {
			Ok(f) => f,
			Err(e) => {
				eprintln!("Failed to open {} for {}: {}",
					  path.display(), sensorcfg.name, e);
				continue;
			},
		};

		let sensor = Sensor::new(arccfg.clone(), &sensorcfg.name, SensorType::Voltage, fd)
			.with_poll_interval(sensorcfg.poll_interval)
			.with_scale(sensorcfg.scale)
			.with_bridge_gpio(bridge_gpio)
			.with_power_state(sensorcfg.power_state);

		// .expect() because we checked for Occupied(Active(_)) earlier
		sensor::install_sensor(entry, dbuspath.clone(), sensor).await
			.expect("sensor magically reactivated?");
	}
	Ok(())
}
