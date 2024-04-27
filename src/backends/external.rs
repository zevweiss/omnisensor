//! Backend providing support for hosting externally-supplied sensor data in dbus objects.
//!
//! A la dbus-sensors's `externalsensor` daemon.

use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
	time::Duration,
};

use tokio::sync::{
	Mutex,
	watch,
};
use log::error;

use crate::{
	DaemonState,
	dbus_helpers::props::*,
	powerstate::PowerState,
	sensor,
	sensor::{
		Sensor,
		SensorConfig,
		SensorMode::ReadWrite,
		SensorType,
	},
	threshold,
	threshold::ThresholdConfig,
	types::*,
};

/// Internal representation of the dbus config data for an external sensor.
#[derive(Debug)]
pub struct ExternalSensorConfig {
	/// Sensor name.
	name: String,
	/// Sensor type.
	kind: SensorType,
	/// Minimum sensor reading value.
	minvalue: f64,
	/// Maximum sensor reading value.
	maxvalue: f64,
	/// Maximum time allowed between updates until data is considered stale.
	timeout: Option<Duration>,
	/// Threshold settings for the sensor.
	thresholds: Vec<ThresholdConfig>,
	/// Host power state in which this sensor is active.
	power_state: PowerState,
}

impl ExternalSensorConfig {
	/// Construct an [`ExternalSensorConfig`] from raw dbus config data.
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str,
	                 intfs: &HashMap<String, dbus::arg::PropMap>) -> ErrResult<Self> {
		let name: &String = prop_get_mandatory(basecfg, "Name")?;
		let kind: &String = prop_get_mandatory(basecfg, "Units")?;
		let minvalue = *prop_get_mandatory(basecfg, "MinValue")?;
		let maxvalue = *prop_get_mandatory(basecfg, "MaxValue")?;
		let timeout = prop_get_optional(basecfg, "Timeout")?.map(|p| Duration::from_secs(*p));
		let thresholds = threshold::get_configs_from_dbus(baseintf, intfs);
		let power_state = prop_get_default_from(basecfg, "PowerState", PowerState::Always)?;

		let Some(kind) = SensorType::from_unit_str(kind) else {
			return Err(err_invalid_data(format!("invalid unit string '{}'", kind)));
		};

		Ok(Self {
			name: name.clone(),
			kind,
			minvalue,
			maxvalue,
			timeout,
			power_state,
			thresholds,
		})
	}
}

/// Core implementation of [`ExternalSensorIO`].
///
/// This simply receives update notifications sent by a callback in
/// the dbus property-set handler path.
struct ExternalSensorIOCore {
	/// Channel by which we get notified of updates.
	rx: watch::Receiver<f64>,
	/// True if the sensor hasn't received an update within its timeout window.
	stale: bool,
}

impl ExternalSensorIOCore {
	/// Construct a new [`ExternalSensorIOCore`].
	fn new(rx: watch::Receiver<f64>) -> Self {
		Self {
			rx,
			stale: true,
		}
	}

	/// Return the most recently sent value for the sensor (or NaN if it's timed out).
	fn read(&self) -> ErrResult<f64> {
		let val = if self.stale {
			f64::NAN
		} else {
			*self.rx.borrow()
		};
		Ok(val)
	}
}

/// "I/O" mechanism for external sensors.
///
/// This is simply a wrapper around [`ExternalSensorIOCore`] so that it can be shared.
pub struct ExternalSensorIO {
	core: Arc<Mutex<ExternalSensorIOCore>>,
}

impl ExternalSensorIO {
	/// Construct a new [`ExternalSensorIO`].
	fn new(rx: watch::Receiver<f64>) -> Self {
		Self { core: Arc::new(Mutex::new(ExternalSensorIOCore::new(rx))) }
	}

	/// Returns the most-recently-sent sensor reading value.
	pub async fn read(&self) -> ErrResult<f64> {
		self.core.lock().await.read()
	}
}

/// Async function that returns when the sensor's value should be updated.
async fn get_next_update(extiocore: Arc<Mutex<ExternalSensorIOCore>>, timeout: Option<Duration>)
{
	let mut extiocore = extiocore.lock().await;
	let update = extiocore.rx.changed();
	if let Some(t) = timeout {
		let res = tokio::time::timeout(t, update).await;
		extiocore.stale = res.is_err();
	} else {
		let res = update.await;
		if let Err(e) = res {
			// FIXME: would be nice to include the sensor name here
			error!("BUG: failed to receive value update: {}", e);
			extiocore.stale = true;
		}
	}
}

/// Instantiate any active external sensors configured in `daemonstate.config`.
pub async fn instantiate_sensors(daemonstate: &DaemonState, dbuspaths: &FilterSet<InventoryPath>,
                                 _retry: &mut HashSet<InventoryPath>) -> ErrResult<()>
{
	let cfgmap = daemonstate.config.lock().await;
	let configs = cfgmap.iter()
		.filter_map(|(path, cfg)| {
			match cfg {
				SensorConfig::External(c) if dbuspaths.contains(path) => Some((path, c)),
				_ => None,
			}
		});
	for (path, extcfg) in configs {
		let mut sensors = daemonstate.sensors.lock().await;

		let Some(entry) = sensor::get_nonactive_sensor_entry(&mut sensors,
		                                                     extcfg.name.clone()).await else {
			continue;
		};

		let (tx, rx) = watch::channel(f64::NAN);
		let extio = ExternalSensorIO::new(rx);
		let extiocore = extio.core.clone();
		let timeout = extcfg.timeout;

		let io = sensor::SensorIO::External(extio);

		let ioctx = sensor::SensorIOCtx::new(io)
			.with_next_update(Box::new(move |_| {
				let extiocore = extiocore.clone();
				Box::pin(async move {
					get_next_update(extiocore, timeout).await;
				})
			}));

		let mode = ReadWrite(Box::new(move |s, v| {
			if let Err(e) = tx.send(v) {
				error!("{}: failed to send value update: {}", s.name, e);
			}
		}));

		let ctor = || {
			Sensor::new(path, &extcfg.name, extcfg.kind, &daemonstate.sensor_intfs,
			            &daemonstate.bus, mode)
				.with_power_state(extcfg.power_state)
				.with_thresholds_from(&extcfg.thresholds, None,
				                      &daemonstate.sensor_intfs.thresholds,
				                      &daemonstate.bus)
				.with_minval(extcfg.minvalue)
				.with_maxval(extcfg.maxvalue)
		};
		sensor::install_or_activate(entry, &daemonstate.crossroads, ioctx,
		                            &daemonstate.sensor_intfs, ctor).await
	}
	Ok(())
}

/// Whether or not the given `cfgtype` is supported by the `external` sensor backend.
pub fn match_cfgtype(cfgtype: &str) -> bool {
	cfgtype == "ExternalSensor"
}
