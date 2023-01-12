//! Backend-independent code for implementing and managing sensors.

use std::{
	collections::HashMap,
	sync::{Arc, Mutex as SyncMutex},
	time::Duration,
};
use dbus::{
	arg::{Arg, RefArg, Append, PropMap},
	nonblock::SyncConnection,
};
use tokio::sync::Mutex;

use crate::{
	DaemonState,
	types::*,
	i2c::I2CDevice,
	gpio::BridgeGPIO,
	powerstate::PowerState,
	sysfs,
	threshold,
	threshold::{
		ThresholdArr,
		ThresholdConfig,
		ThresholdIntfDataArr,
	},
	dbus_helpers::SignalProp,
};

#[cfg(feature = "adc")]
use crate::adc;

#[cfg(feature = "hwmon")]
use crate::hwmon;

#[cfg(feature = "fan")]
use crate::fan;

#[cfg(feature = "peci")]
use crate::peci;

#[cfg(feature = "external")]
use crate::external;

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

	/// Return the sensor type indicated by the given unit string.
	pub fn from_unit_str(unit: &str) -> Option<Self> {
		match unit {
			"DegreesC" => Some(Self::Temperature),
			"RPMS" => Some(Self::RPM),
			"Volts" => Some(Self::Voltage),
			"Amperes" => Some(Self::Current),
			"Watts" => Some(Self::Power),
			_ => None,
		}
	}

	/// Return the category in the dbus sensors hierarchy for a [`SensorType`] (i.e. a
	/// dbus path component).
	pub fn dbus_category(&self) -> &'static str {
		match self {
			Self::Temperature => "temperature",
			Self::RPM => "fan_tach",
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

	/// Return the hwmon type tag (filename prefix) for the sensor type.
	pub fn hwmon_typetag(&self) -> &'static str {
		match self {
			Self::Temperature => "temp",
			Self::RPM => "fan",
			Self::Voltage => "in",
			Self::Current => "curr",
			Self::Power => "power",
		}
	}
}

/// An enum of config data all supported sensor types.
pub enum SensorConfig {
	#[cfg(feature = "hwmon")]
	Hwmon(hwmon::HwmonSensorConfig),

	#[cfg(feature = "adc")]
	ADC(adc::ADCSensorConfig),

	#[cfg(feature = "fan")]
	Fan(fan::FanSensorConfig),

	#[cfg(feature = "peci")]
	PECI(peci::PECISensorConfig),

	#[cfg(feature = "external")]
	External(external::ExternalSensorConfig),
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
	pub fn from_dbus(props: &PropMap, intf: &str,
	                 all_intfs: &HashMap<String, PropMap>) -> Option<ErrResult<Self>> {
		let parts: Vec<&str> = intf.split('.').collect();
		if parts.len() != 4
			|| parts[0] != "xyz"
			|| parts[1] != "openbmc_project"
			|| parts[2] != "Configuration"
		{
			return None;
		}
		let cfgtype = parts[3];

		#[cfg(feature = "adc")]
		if adc::match_cfgtype(cfgtype) {
			return Some(adc::ADCSensorConfig::from_dbus(props, intf, all_intfs)
			            .map(SensorConfig::ADC));
		}

		#[cfg(feature = "hwmon")]
		if hwmon::match_cfgtype(cfgtype) {
			return Some(hwmon::HwmonSensorConfig::from_dbus(props, intf, all_intfs)
			            .map(SensorConfig::Hwmon));
		}

		#[cfg(feature = "fan")]
		if fan::match_cfgtype(cfgtype) {
			return Some(fan::FanSensorConfig::from_dbus(props, intf, all_intfs)
			            .map(SensorConfig::Fan));
		}

		#[cfg(feature = "peci")]
		if peci::match_cfgtype(cfgtype) {
			return Some(peci::PECISensorConfig::from_dbus(props, intf, all_intfs)
			            .map(SensorConfig::PECI));
		}

		#[cfg(feature = "external")]
		if external::match_cfgtype(cfgtype) {
			return Some(external::ExternalSensorConfig::from_dbus(props, intf, all_intfs)
			            .map(SensorConfig::External));
		}

		return Some(Err(err_unsupported(format!("unsupported Configuration type '{}'",
		                                        cfgtype))));
	}
}

/// A map of all sensor configs retrieved from entity-manager.
pub type SensorConfigMap = HashMap<Arc<InventoryPath>, SensorConfig>;

/// An enum of underlying I/O mechanisms used to retrieve sensor readings.
pub enum SensorIO {
	Sysfs(sysfs::SysfsSensorIO),

	#[cfg(feature = "external")]
	External(external::ExternalSensorIO),
}

impl SensorIO {
	/// Read a sample for a sensor.
	async fn read(&mut self) -> ErrResult<f64> {
		match self {
			Self::Sysfs(x) => x.read().await,

			#[cfg(feature = "external")]
			Self::External(x) => x.read().await,
		}
	}
}

/// Type alias for an async function used to determine when a sensor's reading should next be updated.
///
/// The update will occur when the returned future resolves.
type NextUpdateFn = Box<dyn FnMut(&Sensor) -> futures::future::BoxFuture<'static, ()> + Send + Sync>;

/// A [`SensorIO`] plus some surrounding context.
pub struct SensorIOCtx {
	/// The sensor I/O mechanism.
	io: SensorIO,
	/// A GPIO that must be asserted before reading the sensor.
	bridge_gpio: Option<BridgeGPIO>,
	/// A reference to an I2C device associated with the sensor.
	i2cdev: Option<Arc<I2CDevice>>,
	/// Function returning a future to await before performing an update of the sensor's value.
	next_update: NextUpdateFn,
}

/// A running task that updates a sensor's value.
///
/// Wrapped in a struct so that we can stop it in its Drop implementation.
struct SensorIOTask(tokio::task::JoinHandle<()>);

impl Drop for SensorIOTask {
	/// Stops the contained task when the [`SensorIOTask`] goes away.
	fn drop(&mut self) {
		self.0.abort();
	}
}

impl SensorIOCtx {
	/// Create a new sensor I/O context from a given I/O mechanism.
	pub fn new(io: SensorIO) -> Self {
		Self {
			io,
			bridge_gpio: None,
			i2cdev: None,
			next_update: Box::new(|s| Box::pin(tokio::time::sleep(s.poll_interval))),
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

	/// Replace the default (periodic polling) [`next_update`](Self::next_update)
	/// function of a sensor I/O context.
	pub fn with_next_update(mut self, next: NextUpdateFn) -> Self {
		self.next_update = next;
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

/// A sensor's (dbus) read-only or read-write mode.
pub enum SensorMode {
	/// The sensor's value is read-only via dbus.
	ReadOnly,

	/// The sensor's value can be written via dbus.
	///
	/// The boxed callback will be called when a dbus write occurs.
	ReadWrite(Box<dyn Fn(&Sensor, f64) + Send>),
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

	/// The number of failed update attempts since the last successful one.
	errcount: usize,

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
	/// The dbus property that provides the sensor's associations.
	associations: SignalProp<Vec<(String, String, String)>>,

	/// The sensor's dbus RO/RW mode.
	mode: SensorMode,

	/// The sensor's running I/O task.
	///
	/// This is [`Some`] when the sensor is active and [`None`] when it's inactive
	/// (e.g. when the host's current power state doesn't match the sensor's
	/// `power_state`).
	iotask: Option<SensorIOTask>,
}

/// The number of consecutive update errors after which a sensor's
/// Functional attribute is set to false.
const MAX_ERRORS: usize = 5;

impl Sensor {
	/// Construct a new [`Sensor`] with the given parameters.
	///
	/// It will initially be disabled (no running I/O task); it can subsequently be
	/// enabled by a call to its [`activate()`](Sensor::activate) method.
	pub fn new(cfgpath: &InventoryPath, name: &str, kind: SensorType,
	           intfs: &SensorIntfData, conn: &Arc<SyncConnection>, mode: SensorMode) -> Self {
		let badchar = |c: char| !(c.is_ascii_alphanumeric() || c == '_');
		let cleanname = name.replace(badchar, "_");
		let dbuspath = format!("/xyz/openbmc_project/sensors/{}/{}", kind.dbus_category(),
		                       cleanname);
		let dbuspath = Arc::new(SensorPath(dbuspath.into()));
		let value_msgfn = &intfs.value_intf(&mode).msgfns.value;
		let cache = SignalProp::new(f64::NAN, value_msgfn, &dbuspath, conn);
		let minvalue = SignalProp::new(f64::NAN, &intfs.value.msgfns.minvalue,
		                               &dbuspath, conn);
		let maxvalue = SignalProp::new(f64::NAN, &intfs.value.msgfns.maxvalue,
		                               &dbuspath, conn);
		let available = SignalProp::new(false, &intfs.availability.msgfns.available,
		                                &dbuspath, conn);
		let functional = SignalProp::new(true, &intfs.opstatus.msgfns.functional,
		                                 &dbuspath, conn);

		// dbus::strings::Path doesn't have a .parent() method, so momentarily
		// pretend it's a filesystem path...
		let parentpath = match std::path::Path::new(&*cfgpath.0).parent() {
			Some(p) => p,
			None => {
				// Presumably unlikely to ever be hit, but I guess this
				// seems slightly better than just calling .unwrap()...
				eprintln!("Warning: bogus-looking config inventory path '{}', \
				           faking parent", cfgpath.0);
				std::path::Path::new("/xyz/openbmc_project/inventory/system")
			},
		};
		let assoc = ("chassis".to_string(), "all_sensors".to_string(),
		             parentpath.to_string_lossy().into());
		let associations = SignalProp::new(vec![assoc], &intfs.assoc.msgfns.associations,
		                                   &dbuspath, conn);

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
			errcount: 0,
			available,
			functional,
			associations,
			mode,

			iotask: None,
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

		for threshold in self.thresholds.iter_mut() {
			if let Some(t) = threshold {
				t.update(newval);
			}
		}
	}

	/// Start a sensor's update task using the provided `ioctx` and mark it available.
	///
	/// This is an associated function rather than a method on `self` because it needs
	/// to pass a weak reference to itself into the future that gets spawned as
	/// [`Sensor::iotask`], and we need an [`Arc`] for that.
	pub async fn activate(sensor: &Arc<Mutex<Sensor>>, mut ioctx: SensorIOCtx) {
		// Use a weak reference in the update task closure so it doesn't hold a
		// strong reference to the sensor (which would create a reference loop and
		// make it un-droppable)
		let weakref = Arc::downgrade(sensor);

		let update_loop = async move {
			loop {
				let Some(sensor) = weakref.upgrade() else {
					break;
				};

				// Set up the sleep here (but don't wait for it) to
				// schedule the timeout for the next update (so that the
				// interval includes the time spent acquiring the lock and
				// doing the update itself, and hence is the period of the
				// whole cyclic operation
				let next = (ioctx.next_update)(&*sensor.lock().await);

				// Stringify the error message here, because 'dyn Error'
				// isn't Send, so we can't hold the ErrResult across an
				// await.
				let readresult = ioctx.read().await.map_err(|e| e.to_string());

				let mut sensor = sensor.lock().await;

				if sensor.iotask.is_none() {
					eprintln!("BUG: update task running on inactive sensor {}",
					          sensor.name);
					break;
				}

				match readresult {
					Ok(v) => {
						let newval = v * sensor.scale;
						sensor.set_value(newval).await;
						sensor.errcount = 0;
						sensor.functional.set(true);
					},
					Err(e) => {
						if sensor.functional.get() {
							eprintln!("{}: update failed: {}",
							          sensor.name, e);
						}
						sensor.errcount += 1;
						if sensor.errcount == MAX_ERRORS {
							eprintln!("{}: error limit exceeded",
							          sensor.name);
							sensor.functional.set(false);
							sensor.set_value(f64::NAN).await;
						}
					},
				};

				drop(sensor); // Release the lock while we await the next update

				// Wait until it's time for the next update.  Do this
				// after the read instead of before so the first read
				// happens promptly (so we avoid a long wait before the
				// first sample for sensors with large poll intervals).
				next.await;
			}
		};

		// Acquire the lock before spawning the iotask so that its iotask.is_none()
		// check doesn't fail because we haven't set it yet.
		let mut sensor = sensor.lock().await;

		let iotask = SensorIOTask(tokio::spawn(update_loop));

		// It'd be nice to find some way to arrange things such that this error
		// case (and the mirror one in deactivate()) vanished by construction, but
		// I haven't been able to do so thus far...
		if sensor.iotask.replace(iotask).is_some() {
			eprintln!("BUG: re-activating already-active sensor {}", sensor.name);
		}

		sensor.available.set(true)
	}

	/// Stop a sensor's update task and mark it unavailable.
	pub async fn deactivate(&mut self) {
		let oldiotask = self.iotask.take();
		if oldiotask.is_none() {
			eprintln!("BUG: deactivate already-inactive sensor {}", self.name);
		}

		// Could just let this go out of scope, but might as well be explicit
		// (this is what aborts the update task)
		drop(oldiotask);

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
			sensor_intfs.value_intf(&self.mode).token,
			sensor_intfs.availability.token,
			sensor_intfs.opstatus.token,
			sensor_intfs.assoc.token,
		];
		for (threshold, intf) in self.thresholds.iter().zip(&sensor_intfs.thresholds) {
			if threshold.is_some() {
				ifaces.push(intf.token);
			}
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
pub async fn get_nonactive_sensor_entry(sensors: &mut SensorMap, key: String)
                                        -> Option<SensorMapEntry<'_>>
{
	let entry = sensors.entry(key);
	if let SensorMapEntry::Occupied(ref e) = entry {
		if e.get().lock().await.iotask.is_some() {
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
pub async fn install_or_activate<F>(entry: SensorMapEntry<'_>,
                                    cr: &SyncMutex<dbus_crossroads::Crossroads>,
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

/// Construct a property for a sensor interface.
///
/// The value will be retrieved by calling `getter()` on the sensor.
fn build_sensor_property<G, S, V>(b: &mut dbus_crossroads::IfaceBuilder<Arc<Mutex<Sensor>>>,
                                  name: &str, getter: G, setter: Option<S>) -> Box<PropChgMsgFn>
where G: Fn(&Sensor) -> V + Send + Copy + 'static,
      S: Fn(&mut Sensor, V) + Send + Copy + 'static,
      V: RefArg + Arg + Append + for<'a> dbus::arg::Get<'a> + Send + 'static
{
	let pb = b.property(name)
		.get_async(move |mut ctx, sensor| {
			let sensor = sensor.clone();
			async move {
				let value = {
					let s = sensor.lock().await;
					getter(&s)
				};
				ctx.reply(Ok(value))
			}
		});

	let pb = if let Some(setter) = setter {
		pb.set_async(move |mut ctx, sensor, value| {
			let sensor = sensor.clone();
			async move {
				{
					let mut s = sensor.lock().await;
					setter(&mut s, value);
				}

				// Skip signal emission; we assume that the provided
				// setter already takes care of it (presumably via a
				// SignalProp, which includes skipping it if the new value
				// is the same as the old one).
				ctx.reply_noemit(Ok(()));

				// FIXME: is there some better way to make the return type
				// work out here?
				core::marker::PhantomData
			}
		})
	} else {
		pb
	};

	// FIXME: they're not all SignalProps, so .emits_changed_true() isn't really
	// guaranteed for everything
	pb.emits_changed_true()
		.changed_msg_fn()
}

/// Helper function for returning a None of a specific type for use as the (absent) setter
/// to pass to [`build_sensor_property`] for read-only properties.
///
/// For reasons that aren't obvious to me the compiler's type inference can't figure out a
/// bare `None` literal, but is happy with a call to this function instead.
fn no_setter<T>() -> Option<fn(&mut Sensor, T)> {
	None
}

/// A collection of [`PropChgMsgFn`]s for the `xyz.openbmc_project.Sensor.Value`
/// interface.
pub struct ValueIntfMsgFns {
	pub unit: Arc<PropChgMsgFn>,
	pub value: Arc<PropChgMsgFn>,
	pub minvalue: Arc<PropChgMsgFn>,
	pub maxvalue: Arc<PropChgMsgFn>,
}

impl ValueIntfMsgFns {
	/// Construct the `xyz.openbmc_project.Sensor.Value` interface.
	fn build(cr: &mut dbus_crossroads::Crossroads, writable: bool) -> SensorIntf<Self> {
		SensorIntf::build(cr, "xyz.openbmc_project.Sensor.Value", |b| {
			let value_setter = if writable {
				Some(|s: &mut Sensor, v| {
					s.cache.set(v);
					match &s.mode {
						SensorMode::ReadWrite(cb) => cb(s, v),
						SensorMode::ReadOnly => {
							eprintln!("BUG: dbus set ReadOnly sensor {}",
							          s.name);
						},
					};
				})
			} else {
				None
			};
			Self {
				unit: build_sensor_property(b, "Unit",
				                            |s| s.kind.dbus_unit_str().to_string(),
				                            no_setter()).into(),
				value: build_sensor_property(b, "Value", |s| s.cache.get(),
				                             value_setter).into(),
				minvalue: build_sensor_property(b, "MinValue", |s| s.minvalue.get(),
				                                no_setter()).into(),
				maxvalue: build_sensor_property(b, "MaxValue", |s| s.maxvalue.get(),
				                                no_setter()).into(),
			}
		})
	}

}

/// The [`PropChgMsgFn`] for the `xyz.openbmc_project.State.Decorator.Availability`
/// interface.
pub struct AvailabilityIntfMsgFns {
	pub available: Arc<PropChgMsgFn>,
}

impl AvailabilityIntfMsgFns {
	/// Construct the `xyz.openbmc_project.State.Decorator.Availability` interface.
	fn build(cr: &mut dbus_crossroads::Crossroads) -> SensorIntf<Self> {
		SensorIntf::build(cr, "xyz.openbmc_project.State.Decorator.Availability", |b| {
			Self {
				available: build_sensor_property(b, "Available", |s| s.available.get(),
				                                 no_setter()).into(),
			}
		})
	}
}

/// The [`PropChgMsgFn`] for the `xyz.openbmc_project.State.Decorator.OperationalStatus`
/// interface.
pub struct OpStatusIntfMsgFns {
	pub functional: Arc<PropChgMsgFn>,
}

impl OpStatusIntfMsgFns {
	/// Construct the `xyz.openbmc_project.State.Decorator.OperationalStatus` interface.
	fn build(cr: &mut dbus_crossroads::Crossroads) -> SensorIntf<Self> {
		SensorIntf::build(cr, "xyz.openbmc_project.State.Decorator.OperationalStatus", |b| {
			Self {
				functional: build_sensor_property(b, "Functional", |s| s.functional.get(),
				                                  no_setter()).into(),
			}
		})
	}
}

/// The [`PropChgMsgFn`] for the `xyz.openbmc_project.Association.Definitions` interface.
pub struct AssocIntfMsgFns {
	pub associations: Arc<PropChgMsgFn>,
}

impl AssocIntfMsgFns {
	/// Construct the `xyz.openbmc_project.Association.Definitions` interface.
	pub fn build(cr: &mut dbus_crossroads::Crossroads) -> SensorIntf<Self> {
		SensorIntf::build(cr, "xyz.openbmc_project.Association.Definitions", |b| {
			Self {
				associations: build_sensor_property(b, "Associations",
				                                    |s| { s.associations.get_clone() },
				                                    no_setter()).into()
			}
		})
	}
}


/// The aggregate of all the dbus interfaces for a sensor.
pub struct SensorIntfData {
	/// The `xyz.openbmc_project.Sensor.Value` interface.
	pub value: SensorIntf<ValueIntfMsgFns>,
	/// The `xyz.openbmc_project.Sensor.Value` interface, with a writable Value property.
	pub writable_value: SensorIntf<ValueIntfMsgFns>,
	/// The `xyz.openbmc_project.State.Decorator.Availability` interface.
	pub availability: SensorIntf<AvailabilityIntfMsgFns>,
	/// The `xyz.openbmc_project.State.Decorator.OperationalStatus` interface.
	pub opstatus: SensorIntf<OpStatusIntfMsgFns>,
	/// A per-severity-level array of `xyz.openbmc_project.Sensor.Threshold.$everity`
	/// interfaces.
	pub thresholds: threshold::ThresholdIntfDataArr,
	/// The `xyz.openbmc_project.Association.Definitions` interface.
	pub assoc: SensorIntf<AssocIntfMsgFns>,
}

impl SensorIntfData {
	/// Construct [`SensorIntfData`].
	pub fn build(cr: &mut dbus_crossroads::Crossroads) -> Self {
		Self {
			value: ValueIntfMsgFns::build(cr, false),
			writable_value: ValueIntfMsgFns::build(cr, true),
			availability: AvailabilityIntfMsgFns::build(cr),
			opstatus: OpStatusIntfMsgFns::build(cr),
			thresholds: threshold::build_sensor_threshold_intfs(cr),
			assoc: AssocIntfMsgFns::build(cr),
		}
	}

	/// Retrieve the appropriate Value interface for the specified writability.
	pub fn value_intf(&self, mode: &SensorMode) -> &SensorIntf<ValueIntfMsgFns> {
		match mode {
			SensorMode::ReadOnly => &self.value,
			SensorMode::ReadWrite(_) => &self.writable_value,
		}
	}
}

/// Iterate through a sensor map and deactivate all the sensors whose
/// [`power_state`](Sensor::power_state) no longer matches the current host power state.
pub async fn deactivate(sensors: &mut SensorMap) {
	for sensor in sensors.values_mut() {
		let sensor = &mut *sensor.lock().await;
		if sensor.iotask.is_none() { // FIXME: wrap this check or something?
			continue;
		};
		if sensor.power_state.active_now() {
			continue;
		}
		sensor.deactivate().await;
	}
}

/// Instantiate sensors from all backends to match the given `cfg`.
pub async fn instantiate_all(daemonstate: &DaemonState, filter: &FilterSet<InventoryPath>) {
	#[cfg(feature = "adc")]
	adc::instantiate_sensors(daemonstate, filter).await.unwrap_or_else(|e| {
		eprintln!("Failed to instantiate ADC sensors: {}", e);
	});

	#[cfg(feature = "fan")]
	fan::instantiate_sensors(daemonstate, filter).await.unwrap_or_else(|e| {
		eprintln!("Failed to instantiate fan sensors: {}", e);
	});

	#[cfg(feature = "peci")]
	peci::instantiate_sensors(daemonstate, filter).await.unwrap_or_else(|e| {
		eprintln!("Failed to instantiate PECI sensors: {}", e);
	});

	#[cfg(feature = "hwmon")]
	hwmon::instantiate_sensors(daemonstate, filter).await.unwrap_or_else(|e| {
		eprintln!("Failed to instantiate hwmon sensors: {}", e);
	});

	#[cfg(feature = "external")]
	external::instantiate_sensors(daemonstate, filter).await.unwrap_or_else(|e| {
		eprintln!("Failed to instantiate external sensors: {}", e);
	});
}
