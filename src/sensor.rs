//! Backend-independent code for implementing and managing sensors.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex as SyncMutex},
	time::Duration,
};
use dbus::nonblock::SyncConnection;
use strum::IntoEnumIterator;
use tokio::sync::Mutex;

use crate::{
	types::*,
	hwmon,
	i2c,
	i2c::I2CDevice,
	gpio::BridgeGPIO,
	powerstate::PowerState,
	sysfs,
	threshold,
	threshold::{
		ThresholdArr,
		ThresholdConfig,
		ThresholdIntfDataArr,
		ThresholdSeverity,
	},
	dbus_helpers::SignalProp,
};

#[cfg(feature = "adc")]
use crate::adc;

#[cfg(feature = "fan")]
use crate::fan;

#[cfg(feature = "peci")]
use crate::peci;

/// The type of a sensor.
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SensorType {
	Temperature,
	RPM,
	Voltage,
	Current,
	Power,
}

impl SensorType {
	/// Return the dbus string used to represent the unit associated with a [`SensorType`].
	fn dbus_unit_str(&self) -> &'static str {
		match self {
			Self::Temperature => "xyz.openbmc_project.Sensor.Value.Unit.DegreesC",
			Self::RPM => "xyz.openbmc_project.Sensor.Value.Unit.RPMS",
			Self::Voltage => "xyz.openbmc_project.Sensor.Value.Unit.Volts",
			Self::Current => "xyz.openbmc_project.Sensor.Value.Unit.Amperes",
			Self::Power => "xyz.openbmc_project.Sensor.Value.Unit.Watts",
		}
	}

	/// Return the category in the dbus sensors hierarchy for a [`SensorType`] (i.e. a
	/// dbus path component).
	pub fn dbus_category(&self) -> &'static str {
		match self {
			Self::Temperature => "temperature",
			Self::RPM => "fanpwm",
			Self::Voltage => "voltage",
			Self::Current => "current",
			Self::Power => "power",
		}
	}

	/// Return the (multiplicative) scaling factor to convert a raw hwmon reading to
	/// the corresponding natural unit.
	pub fn hwmon_scale(&self) -> f64 {
		const UNIT: f64 = 1.0;
		const MILLI: f64 = 0.001;
		const MICRO: f64 = 0.000001;
		match self {
			Self::Voltage => MILLI,
			Self::RPM => UNIT,
			Self::Temperature => MILLI,
			Self::Current => MILLI,
			Self::Power => MICRO,
		}
	}

	/// Return the sensor type indicated by the given hwmon type tag (filename prefix).
	pub fn from_hwmon_typetag(tag: &str) -> Option<Self> {
		match tag {
			"temp" => Some(Self::Temperature),
			"fan" => Some(Self::RPM),
			"in" => Some(Self::Voltage),
			"curr" => Some(Self::Current),
			"power" => Some(Self::Power),
			_ => None,
		}
	}
}

/// An enum of config data all supported sensor types.
pub enum SensorConfig {
	Hwmon(hwmon::HwmonSensorConfig),

	#[cfg(feature = "adc")]
	ADC(adc::ADCSensorConfig),

	#[cfg(feature = "fan")]
	Fan(fan::FanSensorConfig),

	#[cfg(feature = "peci")]
	PECI(peci::PECISensorConfig),
}

impl SensorConfig {
	/// Construct a [`SensorConfig`] from dbus properties `props`
	///
	/// `props` should be the entry for `intf` in `all_intfs`.
	///
	/// Returns:
	///  * `Some(Ok(_))`: successful parse.
	///  * `Some(Err(_))`: we tried to parse a config, but something was wrong with it.
	///  * `None`: `intf` isn't a sensor configuration, no attempt at parsing.
	pub fn from_dbus(props: &dbus::arg::PropMap, intf: &str, all_intfs: &HashMap<String, dbus::arg::PropMap>) -> Option<ErrResult<Self>> {
		let parts: Vec<&str> = intf.split('.').collect();
		if parts.len() != 4 || parts[0] != "xyz" || parts[1] != "openbmc_project" || parts[2] != "Configuration" {
				return None;
		}
		let cfgtype = parts[3];

		let res = match cfgtype {
			#[cfg(feature = "adc")]
			"ADC" => adc::ADCSensorConfig::from_dbus(props, intf, &all_intfs).map(SensorConfig::ADC),

			"LM25066"|"W83773G"|"NCT6779" => hwmon::HwmonSensorConfig::from_dbus(props, intf, &all_intfs).map(SensorConfig::Hwmon),

			#[cfg(feature = "fan")]
			"AspeedFan" => fan::FanSensorConfig::from_dbus(props, intf, &all_intfs).map(SensorConfig::Fan),

			#[cfg(feature = "peci")]
			"XeonCPU" => peci::PECISensorConfig::from_dbus(props, intf, &all_intfs).map(SensorConfig::PECI),

			_ => {
				println!("\t{}:", intf);
				for (p, v) in props {
					println!("\t\t{}: {:?}", p, v);
				}
				Err(err_unsupported(format!("unsupported Configuration type '{}'", cfgtype)))
			}
		};
		Some(res)
	}
}

/// A map of all sensor configs retrieved from entity-manager.
pub type SensorConfigMap = HashMap<Arc<InventoryPath>, SensorConfig>;

/// An enum of underlying I/O mechanisms used to retrieve sensor readings.
pub enum SensorIO {
	Sysfs(sysfs::SysfsSensorIO),
}

impl SensorIO {
	/// Read a sample for a sensor.
	async fn read(&mut self) -> ErrResult<f64> {
		match self {
			Self::Sysfs(x) => x.read(),
		}.await
	}
}

/// A [`SensorIO`] plus some surrounding context.
pub struct SensorIOCtx {
	/// The sensor I/O mechanism.
	io: SensorIO,
	/// A GPIO that must be asserted before reading the sensor.
	bridge_gpio: Option<BridgeGPIO>,
	/// A reference to an I2C device associated with the sensor.
	i2cdev: Option<Arc<I2CDevice>>,
}

/// A [`SensorIOCtx`] plus a running task for updating it.
struct SensorIOTask {
	/// The sensor I/O context.
	ctx: SensorIOCtx,
	/// A running task periodically sampling the sensor.
	update_task: tokio::task::JoinHandle<()>,
}

impl Drop for SensorIOTask {
	/// Stops [`SensorIOTask::update_task`] when the [`SensorIOTask`] goes away.
	fn drop(&mut self) {
		self.update_task.abort();
	}
}

impl SensorIOCtx {
	/// Create a new sensor I/O context from a given I/O mechanism.
	pub fn new(io: SensorIO) -> Self {
		Self {
			io,
			bridge_gpio: None,
			i2cdev: None,
		}
	}

	/// Add a [`BridgeGPIO`] to a sensor I/O context.
	pub fn with_bridge_gpio(mut self, bridge_gpio: Option<BridgeGPIO>) -> Self {
		self.bridge_gpio = bridge_gpio;
		self
	}

	/// Add an [`I2CDevice`] to a sensor I/O context.
	pub fn with_i2cdev(mut self, i2cdev: Option<Arc<I2CDevice>>) -> Self {
		self.i2cdev = i2cdev;
		self
	}

	/// Take a sample from the sensor, asserting its bridge GPIO if required.
	pub async fn read(&mut self) -> ErrResult<f64> {
		let _gpio_hold = match self.bridge_gpio.as_mut().map(|g| g.activate()) {
			Some(x) => Some(x.await?),
			None => None,
		};
		self.io.read().await
	}
}

/// The top-level representation of a sensor.
pub struct Sensor {
	/// The globally-unique name of the sensor.
	pub name: String,
	/// The dbus path of the sensor object.
	dbuspath: Arc<SensorPath>,
	/// The type of the sensor.
	pub kind: SensorType,
	/// The period of the sensor polling loop.
	poll_interval: Duration,
	/// The host power state in which this sensor is enabled.
	pub power_state: PowerState,
	/// Threshold values (warning, critical, etc.) for the sensor.
	pub thresholds: ThresholdArr,

	/// A multiplicative scaling factor for readings from the sensor.
	///
	/// This is only the config-specified scaling factor (converted to a multiplier);
	/// scaling to convert sysfs hwmon values to the desired units (e.g. 0.001 to
	/// convert a sysfs millivolt value to volts) happens
	/// [elsewhere](sysfs::SysfsSensorIO::read).
	scale: f64,

	/// The dbus property that provides the sensor's value.
	cache: SignalProp<f64>,
	/// The dbus property that provides the sensor's minimum value.
	minvalue: SignalProp<f64>,
	/// The dbus property that provides the sensor's maximum value.
	maxvalue: SignalProp<f64>,
	/// The dbus property that provides the sensor's availability state.
	available: SignalProp<bool>,
	/// The dbus property that provides the sensor's functionality state.
	functional: SignalProp<bool>,

	/// The sensor's running I/O task.
	///
	/// This is [`Some`] when the sensor is active and [`None`] when it's inactive
	/// (e.g. when the host's current power state doesn't match the sensor's
	/// `power_state`).
	io: Option<SensorIOTask>,
}

impl Sensor {
	/// Construct a new [`Sensor`] with the given parameters.
	///
	/// It will initially be disabled (no running I/O task); it can subsequently be
	/// enabled by a call to its [`activate()`](Sensor::activate) method.
	pub fn new(name: &str, kind: SensorType, intfs: &SensorIntfData, conn: &Arc<SyncConnection>) -> Self {
		let badchar = |c: char| !(c.is_ascii_alphanumeric() || c == '_');
		let cleanname = name.replace(badchar, "_");
		let dbuspath = format!("/xyz/openbmc_project/sensors/{}/{}", kind.dbus_category(), cleanname);
		let dbuspath = Arc::new(SensorPath(dbuspath.into()));
		let cache = SignalProp::new(f64::NAN, &intfs.value.msgfns.value, &dbuspath, conn);
		let minvalue = SignalProp::new(f64::NAN, &intfs.value.msgfns.minvalue, &dbuspath, conn);
		let maxvalue = SignalProp::new(f64::NAN, &intfs.value.msgfns.maxvalue, &dbuspath, conn);
		let available = SignalProp::new(false, &intfs.availability.msgfns.available, &dbuspath, conn);
		let functional = SignalProp::new(true, &intfs.opstatus.msgfns.functional, &dbuspath, conn);

		Self {
			name: name.into(),
			dbuspath,
			kind,
			cache,
			minvalue,
			maxvalue,
			poll_interval: Duration::from_secs(1),
			power_state: PowerState::Always,
			thresholds: ThresholdArr::default(),
			scale: 1.0,
			available,
			functional,

			io: None,
		}
	}

	/// Set the sensor's [`poll_interval`](Sensor::poll_interval).
	pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
		self.poll_interval = poll_interval;
		self
	}

	/// Set the sensor's [`power_state`](Sensor::power_state).
	pub fn with_power_state(mut self, power_state: PowerState) -> Self {
		self.power_state = power_state;
		self
	}

	/// Set the sensor's [`thresholds`](Sensor::thresholds).
	///
	/// The thresholds are constructed from the provided config data, interfaces, and
	/// dbus connection.
	pub fn with_thresholds_from(mut self, cfg: &[ThresholdConfig],
				    threshold_intfs: &ThresholdIntfDataArr,
				    conn: &Arc<SyncConnection>) -> Self {
		self.thresholds = threshold::get_thresholds_from_configs(cfg, threshold_intfs,
									 &self.dbuspath, conn);
		self
	}

	/// Set the sensor's [`scale`](Sensor::scale).
	pub fn with_scale(mut self, scale: f64) -> Self {
		self.scale = scale;
		self
	}

	/// Set the sensor's [`maxvalue`](Sensor::maxvalue).
	pub fn with_maxval(mut self, max: f64) -> Self {
		self.maxvalue.set(max);
		self
	}

	/// Set the sensor's [`minvalue`](Sensor::minvalue).
	pub fn with_minval(mut self, min: f64) -> Self {
		self.minvalue.set(min);
		self
	}

	/// Set the sensor's [`cache`](Sensor::cache)d value and update the state of its
	/// [`thresholds`](Sensor::thresholds).
	async fn set_value(&mut self, newval: f64) {
		self.cache.set(newval);

		for sev in ThresholdSeverity::iter() {
			if let Some(t) = &mut self.thresholds[sev as usize] {
				t.update(newval);
			}
		}
	}

	/// Read a sample from the sensor and perform the ensuing state updates.
	async fn update(&mut self) -> ErrResult<()> {
		if let Some(io) = &mut self.io {
			let val = io.ctx.read().await?;
			self.set_value(val * self.scale).await;
			Ok(())
		} else {
			Err(err_not_found("update() called on inactive sensor"))
		}
	}

	/// Start a sensor's update task using the provided `ioctx` and mark it available.
	///
	/// This is an associated function rather than a method on `self` because it needs
	/// to pass a weak reference to itself into the future that gets spawned as
	/// [`update_task`](SensorIOTask::update_task), and we need an [`Arc`] for that.
	pub async fn activate(sensor: &Arc<Mutex<Sensor>>, ioctx: SensorIOCtx) {
		let mut s = sensor.lock().await;
		let poll_interval = s.poll_interval;

		// Use a weak reference in the update task closure so it doesn't hold a
		// strong reference to the sensor (which would create a reference loop via
		// s.update_task and make it un-droppable)
		let weakref = Arc::downgrade(&sensor);

		let update_loop = async move {
			let mut poll_interval = poll_interval;
			loop {
				let Some(sensor) = weakref.upgrade() else {
					break;
				};

				// Create the sleep here to schedule the timeout but don't
				// wait for it (so that the interval includes the time
				// spent acquiring the lock and doing the update itself,
				// and hence is the period of the whole cyclic operation).
				let sleep = tokio::time::sleep(poll_interval);

				let mut sensor = sensor.lock().await;

				if sensor.io.is_some() {
					if let Err(e) = sensor.update().await {
						eprintln!("failed to update {}: {}", sensor.name, e);
					}
				} else {
					eprintln!("BUG: update task running on inactive sensor");
				}

				// Read the poll interval for the sleep on the next
				// iteration of the loop
				poll_interval = sensor.poll_interval;

				drop(sensor); // Release the lock while we sleep

				// Now await the sleep.  Do this after the read instead of
				// before so the first read happens promptly (so we avoid
				// a long wait before the first sample for sensors with
				// large poll intervals)
				sleep.await;
			}
		};

		let io = SensorIOTask {
			ctx: ioctx,
			update_task: tokio::spawn(update_loop),
		};

		// It'd be nice to find some way to arrange things such that this error
		// case (and the mirror one in deactivate()) vanished by construction, but
		// I haven't been able to do so thus far...
		if s.io.replace(io).is_some() {
			eprintln!("BUG: re-activating already-active sensor {}", s.name);
		}

		s.available.set(true)
	}

	/// Stop a sensor's update task and mark it unavailable.
	pub async fn deactivate(&mut self) {
		let oldio = self.io.take();
		if oldio.is_none() {
			eprintln!("BUG: deactivate already-inactive sensor {}", self.name);
		}

		// Could just let this go out of scope, but might as well be explicit
		// (this is what aborts the update task)
		drop(oldio);

		self.set_value(f64::NAN).await;
		self.available.set(false)
	}

	/// Publish a sensor's properties on dbus.
	///
	/// `cbdata` must contain `self`.  (Perhaps this should instead be an associated
	/// function that just takes that instead of both separately.)
	pub fn add_to_dbus(&self, cr: &SyncMutex<dbus_crossroads::Crossroads>,
			   sensor_intfs: &SensorIntfData, cbdata: &Arc<Mutex<Sensor>>)
	{
		let mut ifaces = vec![
			sensor_intfs.value.token,
			sensor_intfs.availability.token,
			sensor_intfs.opstatus.token,
		];
		for sev in ThresholdSeverity::iter() {
			if self.thresholds[sev as usize].is_none() {
				continue;
			}
			let intfdata = &sensor_intfs.thresholds[sev as usize];
			ifaces.push(intfdata.token);
		}
		cr.lock().unwrap().insert(self.dbuspath.0.clone(), &ifaces, cbdata.clone());
	}
}

/// Maps sensor names to Sensors.
pub type SensorMap = HashMap<String, Arc<Mutex<Sensor>>>;
/// A convenience alias for use with [`get_nonactive_sensor_entry()`] and [`install_or_activate()`].
pub type SensorMapEntry<'a> = std::collections::hash_map::Entry<'a, String, Arc<Mutex<Sensor>>>;

/// Find a [`SensorMap`] entry for the given `key`.
///
/// Returns [`Some`] if there was no previous entry for `key` or if the sensor for `key`
/// is inactive.  If there is an active sensor for `key`, returns [`None`].
pub async fn get_nonactive_sensor_entry(sensors: &mut SensorMap, key: String) -> Option<SensorMapEntry<'_>>
{
	let entry = sensors.entry(key);
	if let SensorMapEntry::Occupied(ref e) = entry {
		if e.get().lock().await.io.is_some() {
			return None;
		}
	}
	Some(entry)
}

/// Given a [`SensorMapEntry`] from [`get_nonactive_sensor_entry()`], either activate the
/// inactive sensor or instantiate a new one.
///
/// If needed (there's no existing inactive sensor), a new sensor is constructed by
/// calling `ctor()`, added to dbus, and inserted into `entry`.  In either case, the
/// sensor is activated with `io` as its I/O context.
pub async fn install_or_activate<F>(entry: SensorMapEntry<'_>, cr: &SyncMutex<dbus_crossroads::Crossroads>,
				    io: SensorIOCtx, sensor_intfs: &SensorIntfData, ctor: F)
	where F: FnOnce() -> Sensor
{
	match entry {
		SensorMapEntry::Vacant(e) => {
			let sensor = Arc::new(Mutex::new(ctor()));
			Sensor::activate(&sensor, io).await;
			sensor.lock().await.add_to_dbus(cr, sensor_intfs, &sensor);
			e.insert(sensor);
		},
		SensorMapEntry::Occupied(e) => {
			// FIXME: update sensor config from hwmcfg
			Sensor::activate(e.get(), io).await;
		},
	};
}

/// Build a sensor dbus interface called `intf`.
///
/// The properties of the interface are constructed by calling `mkprops()`, which returns
/// a struct of [`PropChgMsgFn`]s (e.g. [`ValueIntfMsgFns`]), which are returned in
/// combination with the [`token`](dbus_crossroads::IfaceToken) created for the interface.
pub fn build_intf<T, F, I>(cr: &mut dbus_crossroads::Crossroads, intf: I, mkprops: F) -> SensorIntf<T>
	where F: FnOnce(&mut dbus_crossroads::IfaceBuilder<Arc<Mutex<Sensor>>>) -> T, I: Into<dbus::strings::Interface<'static>>
{
	let mut msgfns: Option<T> = None;
	let token = cr.register(intf, |b: &mut dbus_crossroads::IfaceBuilder<Arc<Mutex<Sensor>>>| {
		msgfns = Some(mkprops(b))
	});

	SensorIntf {
		token,
		msgfns: msgfns.expect("no msgfns set?"),
	}
}

/// A collection of [`PropChgMsgFn`]s for the `xyz.openbmc_project.Sensor.Value`
/// interface.
pub struct ValueIntfMsgFns {
	pub unit: Arc<PropChgMsgFn>,
	pub value: Arc<PropChgMsgFn>,
	pub minvalue: Arc<PropChgMsgFn>,
	pub maxvalue: Arc<PropChgMsgFn>,
}

/// Construct a property for a sensor interface.
///
/// The value will be retrieved by calling `getter()` on the sensor.
fn build_sensor_property<F, R>(b: &mut dbus_crossroads::IfaceBuilder<Arc<Mutex<Sensor>>>, name: &str, getter: F) -> Box<PropChgMsgFn>
where F: Fn(&Sensor) -> R + Send + Copy + 'static, R: dbus::arg::RefArg + dbus::arg::Arg + dbus::arg::Append + Send + 'static
{
	b.property(name)
		.get_async(move |mut ctx, sensor| {
			let sensor = sensor.clone();
			async move {
				let s = sensor.lock().await;
				ctx.reply(Ok(getter(&s)))
			}
		})
		.emits_changed_true() // FIXME: this isn't guaranteed for everything (they're not all SignalProps)
		.changed_msg_fn()
}

/// Construct the `xyz.openbmc_project.Sensor.Value` interface.
fn build_sensor_value_intf(cr: &mut dbus_crossroads::Crossroads) -> SensorIntf<ValueIntfMsgFns> {
	build_intf(cr, "xyz.openbmc_project.Sensor.Value", |b| {
		ValueIntfMsgFns {
			unit: build_sensor_property(b, "Unit", |s| s.kind.dbus_unit_str().to_string()).into(),
			value: build_sensor_property(b, "Value", |s| s.cache.get()).into(),
			minvalue: build_sensor_property(b, "MinValue", |s| s.minvalue.get()).into(),
			maxvalue: build_sensor_property(b, "MaxValue", |s| s.maxvalue.get()).into(),
		}
	})
}

/// The [`PropChgMsgFn`] for the `xyz.openbmc_project.State.Decorator.Availability`
/// interface.
pub struct AvailabilityIntfMsgFns {
	pub available: Arc<PropChgMsgFn>,
}

/// Construct the `xyz.openbmc_project.State.Decorator.Availability` interface.
fn build_availability_intf(cr: &mut dbus_crossroads::Crossroads) -> SensorIntf<AvailabilityIntfMsgFns> {
	build_intf(cr, "xyz.openbmc_project.State.Decorator.Availability", |b| {
		AvailabilityIntfMsgFns {
			available: build_sensor_property(b, "Available", |s| s.available.get()).into(),
		}
	})
}

/// The [`PropChgMsgFn`] for the `xyz.openbmc_project.State.Decorator.OperationalStatus`
/// interface.
pub struct OpStatusIntfMsgFns {
	pub functional: Arc<PropChgMsgFn>,
}

/// Construct the `xyz.openbmc_project.State.Decorator.OperationalStatus` interface.
fn build_opstatus_intf(cr: &mut dbus_crossroads::Crossroads) -> SensorIntf<OpStatusIntfMsgFns> {
	build_intf(cr, "xyz.openbmc_project.State.Decorator.OperationalStatus", |b| {
		OpStatusIntfMsgFns {
			functional: build_sensor_property(b, "Functional", |s| s.functional.get()).into(),
		}
	})
}

/// The aggregate of all the dbus interfaces for a sensor.
pub struct SensorIntfData {
	/// The `xyz.openbmc_project.Sensor.Value` interface.
	pub value: SensorIntf<ValueIntfMsgFns>,
	/// The `xyz.openbmc_project.State.Decorator.Availability` interface.
	pub availability: SensorIntf<AvailabilityIntfMsgFns>,
	/// The `xyz.openbmc_project.State.Decorator.OperationalStatus` interface.
	pub opstatus: SensorIntf<OpStatusIntfMsgFns>,
	/// A per-severity-level array of `xyz.openbmc_project.Sensor.Threshold.$severity`
	/// interfaces.
	pub thresholds: threshold::ThresholdIntfDataArr,
}

/// Construct [`SensorIntfData`].
pub fn build_sensor_intfs(cr: &mut dbus_crossroads::Crossroads) -> SensorIntfData {
	SensorIntfData {
		value: build_sensor_value_intf(cr),
		availability: build_availability_intf(cr),
		opstatus: build_opstatus_intf(cr),
		thresholds: threshold::build_sensor_threshold_intfs(cr),
	}
}

/// Iterate through a sensor map and deactivate all the sensors whose
/// [`power_state`](Sensor::power_state) no longer matches the current host power state.
pub async fn deactivate(sensors: &mut SensorMap) {
	for sensor in sensors.values_mut() {
		let sensor = &mut *sensor.lock().await;
		if sensor.io.is_none() { // FIXME: wrap this check or something?
			continue;
		};
		if sensor.power_state.active_now() {
			continue;
		}
		sensor.deactivate().await;
	}
}

/// Instantiate sensors from all backends to match the given `cfg`.
pub async fn instantiate_all(cfg: &Mutex<SensorConfigMap>, sensors: &Mutex<SensorMap>,
			     filter: &FilterSet<InventoryPath>, i2cdevs: &Mutex<i2c::I2CDeviceMap>,
			     cr: &SyncMutex<dbus_crossroads::Crossroads>, conn: &Arc<SyncConnection>, intfs: &SensorIntfData) {
	let cfg = cfg.lock().await;
	let mut sensors = sensors.lock().await;

	#[cfg(feature = "adc")]
	adc::instantiate_sensors(&cfg, &mut sensors, filter, cr, conn, intfs).await.unwrap_or_else(|e| {
		eprintln!("Failed to instantiate ADC sensors: {}", e);
	});

	#[cfg(feature = "fan")]
	fan::instantiate_sensors(&cfg, &mut sensors, filter, cr, conn, intfs).await.unwrap_or_else(|e| {
		eprintln!("Failed to instantiate fan sensors: {}", e);
	});

	#[cfg(feature = "peci")]
	peci::instantiate_sensors(&cfg, &mut sensors, filter, cr, conn, intfs).await.unwrap_or_else(|e| {
		eprintln!("Failed to instantiate PECI sensors: {}", e);
	});

	let mut i2cdevs = i2cdevs.lock().await;
	hwmon::instantiate_sensors(&cfg, &mut sensors, filter, &mut i2cdevs, cr, intfs, conn).await.unwrap_or_else(|e| {
		eprintln!("Failed to instantiate hwmon sensors: {}", e);
	});
}
