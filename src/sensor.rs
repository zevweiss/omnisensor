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
	dbus_helpers::AutoProp,
};

#[cfg(feature = "adc")]
use crate::adc;

#[cfg(feature = "peci")]
use crate::peci;

#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub enum SensorType {
	Temperature,
	RPM,
	Voltage,
	Current,
	Power,
}

impl SensorType {
	fn dbus_unit_str(&self) -> &'static str {
		match self {
			Self::Temperature => "xyz.openbmc_project.Sensor.Value.Unit.DegreesC",
			Self::RPM => "xyz.openbmc_project.Sensor.Value.Unit.RPMS",
			Self::Voltage => "xyz.openbmc_project.Sensor.Value.Unit.Volts",
			Self::Current => "xyz.openbmc_project.Sensor.Value.Unit.Amperes",
			Self::Power => "xyz.openbmc_project.Sensor.Value.Unit.Watts",
		}
	}

	pub fn dbus_category(&self) -> &'static str {
		match self {
			Self::Temperature => "temperature",
			Self::RPM => "fanpwm",
			Self::Voltage => "voltage",
			Self::Current => "current",
			Self::Power => "power",
		}
	}

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

pub enum SensorConfig {
	Hwmon(hwmon::HwmonSensorConfig),

	#[cfg(feature = "adc")]
	ADC(adc::ADCSensorConfig),

	#[cfg(feature = "peci")]
	PECI(peci::PECISensorConfig),
}

pub type SensorConfigMap = HashMap<Arc<InventoryPath>, SensorConfig>;

pub enum SensorIO {
	Sysfs(sysfs::SysfsSensorIO),
}

impl SensorIO {
	async fn read(&mut self) -> ErrResult<f64> {
		match self {
			Self::Sysfs(x) => x.read(),
		}.await
	}
}

pub struct SensorIOCtx {
	io: SensorIO,
	bridge_gpio: Option<BridgeGPIO>,
	i2cdev: Option<Arc<I2CDevice>>,
}

struct SensorIOTask {
	ctx: SensorIOCtx,
	update_task: tokio::task::JoinHandle<()>,
}

impl Drop for SensorIOTask {
	fn drop(&mut self) {
		self.update_task.abort();
	}
}

impl SensorIOCtx {
	pub fn new(io: SensorIO) -> Self {
		Self {
			io,
			bridge_gpio: None,
			i2cdev: None,
		}
	}

	pub fn with_bridge_gpio(mut self, bridge_gpio: Option<BridgeGPIO>) -> Self {
		self.bridge_gpio = bridge_gpio;
		self
	}

	pub fn with_i2cdev(mut self, i2cdev: Option<Arc<I2CDevice>>) -> Self {
		self.i2cdev = i2cdev;
		self
	}

	pub async fn read(&mut self) -> ErrResult<f64> {
		let _gpio_hold = match self.bridge_gpio.as_ref().map(|g| g.activate()) {
			Some(x) => Some(x.await?),
			None => None,
		};
		self.io.read().await
	}
}

pub struct Sensor {
	pub name: String,
	dbuspath: Arc<SensorPath>,
	pub kind: SensorType,
	poll_interval: Duration,
	pub power_state: PowerState,
	pub thresholds: ThresholdArr,

	// This is only the config-specified scaling factor (converted
	// to a multiplier); scaling to convert sysfs hwmon values to
	// the desired units (e.g. 0.001 to convert a sysfs millivolt
	// value to volts) happens elsewhere.
	scale: f64,

	cache: AutoProp<f64>,
	minvalue: AutoProp<f64>,
	maxvalue: AutoProp<f64>,
	available: AutoProp<bool>,
	functional: AutoProp<bool>,

	io: Option<SensorIOTask>,
}

impl Sensor {
	pub fn new(name: &str, kind: SensorType, intfs: &SensorIntfData, conn: &Arc<SyncConnection>) -> Self {
		let badchar = |c: char| !(c.is_ascii_alphanumeric() || c == '_');
		let cleanname = name.replace(badchar, "_");
		let dbuspath = format!("/xyz/openbmc_project/sensors/{}/{}", kind.dbus_category(), cleanname);
		let dbuspath = Arc::new(SensorPath(dbuspath.into()));
		let cache = AutoProp::new(f64::NAN, &intfs.value.msgfns.value, &dbuspath, conn);
		let minvalue = AutoProp::new(f64::NAN, &intfs.value.msgfns.minvalue, &dbuspath, conn);
		let maxvalue = AutoProp::new(f64::NAN, &intfs.value.msgfns.maxvalue, &dbuspath, conn);
		let available = AutoProp::new(false, &intfs.availability.msgfns.available, &dbuspath, conn);
		let functional = AutoProp::new(true, &intfs.opstatus.msgfns.functional, &dbuspath, conn);

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

	pub fn with_poll_interval(mut self, poll_interval: Duration) -> Self {
		self.poll_interval = poll_interval;
		self
	}

	pub fn with_power_state(mut self, power_state: PowerState) -> Self {
		self.power_state = power_state;
		self
	}

	pub fn with_thresholds_from(mut self, cfg: &[ThresholdConfig],
				    threshold_intfs: &ThresholdIntfDataArr,
				    conn: &Arc<SyncConnection>) -> Self {
		self.thresholds = threshold::get_thresholds_from_configs(cfg, threshold_intfs,
									 &self.dbuspath, conn);
		self
	}

	pub fn with_scale(mut self, scale: f64) -> Self {
		self.scale = scale;
		self
	}

	pub fn with_maxval(mut self, max: f64) -> Self {
		self.maxvalue.set(max);
		self
	}

	pub fn with_minval(mut self, min: f64) -> Self {
		self.minvalue.set(min);
		self
	}

	async fn set_value(&mut self, newval: f64) {
		self.cache.set(newval);

		for sev in ThresholdSeverity::iter() {
			if let Some(t) = &mut self.thresholds[sev as usize] {
				t.update(newval);
			}
		}
	}

	async fn update(&mut self) -> ErrResult<()> {
		if let Some(io) = &mut self.io {
			let val = io.ctx.read().await?;
			self.set_value(val * self.scale).await;
			Ok(())
		} else {
			Err(Box::new(std::io::Error::new(std::io::ErrorKind::NotFound,
							 "update() called on inactive sensor")))
		}
	}

	pub async fn activate(sensor: &Arc<Mutex<Sensor>>, ioctx: SensorIOCtx) {
		let mut s = sensor.lock().await;
		let poll_interval = s.poll_interval;

		// Use a weak reference in the update task closure so
		// it doesn't hold a strong reference to the sensor
		// (which would create a reference loop via
		// s.update_task and make it un-droppable)
		let weakref = Arc::downgrade(&sensor);

		let update_loop = async move {
			let mut poll_interval = poll_interval;
			loop {
				let Some(sensor) = weakref.upgrade() else {
					break;
				};

				// Create the sleep here to schedule the timeout
				// but don't wait for it (so that the interval
				// includes the time spent acquiring the lock
				// and doing the update itself, and hence is the
				// period of the whole cyclic operation).
				let sleep = tokio::time::sleep(poll_interval);

				let mut sensor = sensor.lock().await;

				if sensor.io.is_some() {
					if let Err(e) = sensor.update().await {
						eprintln!("failed to update {}: {}", sensor.name, e);
					}
				} else {
					eprintln!("BUG: update task running on inactive sensor");
				}

				// Read the poll interval for the sleep on the
				// next iteration of the loop
				poll_interval = sensor.poll_interval;

				drop(sensor); // Release the lock while we sleep

				// Now await the sleep.  Do this after the read
				// instead of before so the first read happens
				// promptly (so we avoid a long wait before the
				// first sample for sensors with large poll
				// intervals)
				sleep.await;
			}
		};

		let io = SensorIOTask {
			ctx: ioctx,
			update_task: tokio::spawn(update_loop),
		};

		if s.io.replace(io).is_some() {
			eprintln!("BUG: re-activating already-active sensor {}", s.name);
		}

		s.available.set(true)
	}

	pub async fn deactivate(&mut self) {
		let oldio = self.io.take();
		if oldio.is_none() {
			eprintln!("BUG: deactivate already-inactive sensor {}", self.name);
		}

		// Could just let this go out of scope, but might as well be
		// explicit (this is what aborts the update task)
		drop(oldio);

		self.set_value(f64::NAN).await;
		self.available.set(false)
	}

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

// Maps sensor names to Sensors
pub type SensorMap = HashMap<String, Arc<Mutex<Sensor>>>;
pub type SensorMapEntry<'a> = std::collections::hash_map::Entry<'a, String, Arc<Mutex<Sensor>>>;

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

pub struct ValueIntfMsgFns {
	pub unit: Arc<PropChgMsgFn>,
	pub value: Arc<PropChgMsgFn>,
	pub minvalue: Arc<PropChgMsgFn>,
	pub maxvalue: Arc<PropChgMsgFn>,
}

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
		.emits_changed_true() // FIXME: this isn't guaranteed for everything (they're not all AutoProps)
		.changed_msg_fn()
}

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

pub struct AvailabilityIntfMsgFns {
	pub available: Arc<PropChgMsgFn>,
}

fn build_availability_intf(cr: &mut dbus_crossroads::Crossroads) -> SensorIntf<AvailabilityIntfMsgFns> {
	build_intf(cr, "xyz.openbmc_project.State.Decorator.Availability", |b| {
		AvailabilityIntfMsgFns {
			available: build_sensor_property(b, "Available", |s| s.available.get()).into(),
		}
	})
}

pub struct OpStatusIntfMsgFns {
	pub functional: Arc<PropChgMsgFn>,
}

fn build_opstatus_intf(cr: &mut dbus_crossroads::Crossroads) -> SensorIntf<OpStatusIntfMsgFns> {
	build_intf(cr, "xyz.openbmc_project.State.Decorator.OperationalStatus", |b| {
		OpStatusIntfMsgFns {
			functional: build_sensor_property(b, "Functional", |s| s.functional.get()).into(),
		}
	})
}

pub struct SensorIntfData {
	pub value: SensorIntf<ValueIntfMsgFns>,
	pub availability: SensorIntf<AvailabilityIntfMsgFns>,
	pub opstatus: SensorIntf<OpStatusIntfMsgFns>,
	pub thresholds: threshold::ThresholdIntfDataArr,
}

pub fn build_sensor_intfs(cr: &mut dbus_crossroads::Crossroads) -> SensorIntfData {
	SensorIntfData {
		value: build_sensor_value_intf(cr),
		availability: build_availability_intf(cr),
		opstatus: build_opstatus_intf(cr),
		thresholds: threshold::build_sensor_threshold_intfs(cr),
	}
}

pub async fn deactivate(sensors: &mut SensorMap) {
	for sensor in sensors.values_mut() {
		let sensor = &mut *sensor.lock().await;
		if sensor.io.is_none() { // FIXME: wrap this check or something?
			continue;
		};
		if sensor.power_state.active_now().await {
			continue;
		}
		sensor.deactivate().await;
	}
}

pub async fn update_all(cfg: &Mutex<SensorConfigMap>, sensors: &Mutex<SensorMap>,
			filter: &FilterSet<InventoryPath>, i2cdevs: &Mutex<i2c::I2CDeviceMap>,
			cr: &SyncMutex<dbus_crossroads::Crossroads>, conn: &Arc<SyncConnection>, intfs: &SensorIntfData) {
	let cfg = cfg.lock().await;
	let mut sensors = sensors.lock().await;

	#[cfg(feature = "adc")]
	adc::update_sensors(&cfg, &mut sensors, filter, cr, conn, intfs).await.unwrap_or_else(|e| {
		eprintln!("Failed to update ADC sensors: {}", e);
	});

	#[cfg(feature = "peci")]
	peci::update_sensors(&cfg, &mut sensors, filter, cr, conn, intfs).await.unwrap_or_else(|e| {
		eprintln!("Failed to update PECI sensors: {}", e);
	});

	let mut i2cdevs = i2cdevs.lock().await;
	hwmon::update_sensors(&cfg, &mut sensors, filter, &mut i2cdevs, cr, intfs, conn).await.unwrap_or_else(|e| {
		eprintln!("Failed to update hwmon sensors: {}", e);
	});
}
