use std::{
	collections::HashMap,
	io::{Read, Seek},
	sync::Arc,
	time::Duration,
};
use dbus::nonblock::SyncConnection;
use tokio::sync::Mutex;

use crate::{
	types::*,
	adc,
	hwmon,
	i2c,
	i2c::I2CDevice,
	gpio::BridgeGPIO,
	powerstate::PowerState,
	threshold,
	threshold::Thresholds,
	dbus_helpers::AutoProp,
};

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

	fn hwmon_scale(&self) -> f64 {
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
	ADC(adc::ADCSensorConfig),
	Hwmon(hwmon::HwmonSensorConfig),
}

pub type SensorConfigMap = HashMap<Arc<dbus::Path<'static>>, SensorConfig>;

pub struct SensorIO {
	fd: std::fs::File,
	bridge_gpio: Option<BridgeGPIO>,
	i2cdev: Option<Arc<I2CDevice>>,
}

struct SensorIOCtx {
	io: SensorIO,
	update_task: tokio::task::JoinHandle<()>,
}

impl Drop for SensorIOCtx {
	fn drop(&mut self) {
		self.update_task.abort();
	}
}

impl SensorIO {
	pub fn new(fd: std::fs::File) -> Self {
		Self {
			fd,
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

	async fn read_raw(&mut self) -> ErrResult<i32> {
		let _gpio_hold = match self.bridge_gpio.as_ref().map(|g| g.activate()) {
			Some(x) => match x.await {
				Ok(h) => Some(h),
				Err(e) => {
					return Err(e);
				},
			},
			None => None,
		};

		read_from_sysfs::<i32>(&mut self.fd)
	}
}

pub struct Sensor {
	pub name: String,
	pub kind: SensorType,
	poll_interval: Duration,
	pub power_state: PowerState,
	pub thresholds: Thresholds,

	// This is a combined (multiplicative) scale factor set to the
	// product of the innate hwmon scaling factor (e.g. 0.001 to
	// convert a sysfs millivolt value to volts) and any optional
	// additional scaling factor specified in the sensor config
	// (e.g. to translate the raw value of a voltage-divided ADC
	// line back to its "real" 12V range or the like).
	scale: f64,

	cache: AutoProp<f64>,

	io: Option<SensorIOCtx>,
}

impl Sensor {
	pub fn new(name: &str, kind: SensorType, intfs: &SensorIntfData,
		   dbuspath: &Arc<dbus::Path<'static>>, conn: &Arc<SyncConnection>) -> Self {
		Self {
			name: name.into(),
			kind,
			cache: AutoProp::new(f64::NAN, &intfs.value.msgfns.value, dbuspath, conn),
			poll_interval: Duration::from_secs(1),
			power_state: PowerState::Always,
			thresholds: Thresholds::new(),
			scale: kind.hwmon_scale(),

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

	pub fn with_thresholds(mut self, thresholds: Thresholds) -> Self {
		self.thresholds = thresholds;
		self
	}

	pub fn with_scale(mut self, scale: f64) -> Self {
		self.scale = scale * self.kind.hwmon_scale();
		self
	}

	async fn set_value(&mut self, newval: f64) {
		self.cache.set(newval);

		for t in self.thresholds.values_mut() {
			t.update(newval);
		}
	}

	async fn update(&mut self) -> ErrResult<()> {
		if let Some(ioctx) = &mut self.io {
			let ival = ioctx.io.read_raw().await?;
			self.set_value((ival as f64) * self.scale).await;
			Ok(())
		} else {
			Err(Box::new(std::io::Error::new(std::io::ErrorKind::NotFound,
							 "update() called on inactive sensor")))
		}
	}

	pub async fn activate(sensor: &Arc<Mutex<Sensor>>, io: SensorIO) {
		let mut s = sensor.lock().await;

		// Use a weak reference in the update task closure so
		// it doesn't hold a strong reference to the sensor
		// (which would create a reference loop via
		// s.update_task and make it un-droppable)
		let weakref = Arc::downgrade(&sensor);

		let update_loop = async move {
			loop {
				let Some(sensor) = weakref.upgrade() else {
					break;
				};

				let mut sensor = sensor.lock().await;

				// Create the sleep here to schedule the timeout
				// but don't wait for it (so that the interval
				// includes the time spent doing the update
				// itself, and hence is the period of the whole
				// cyclic operation).  FIXME: it does still
				// include the time spent acquiring the lock,
				// but I dunno if there's much we can do about
				// that, realistically...
				let sleep = tokio::time::sleep(sensor.poll_interval);

				if sensor.io.is_some() {
					if let Err(e) = sensor.update().await {
						eprintln!("failed to update {}: {}", sensor.name, e);
					}
				} else {
					eprintln!("BUG: update task running on inactive sensor");
				}

				drop(sensor); // Release the lock while we sleep

				// Now await the sleep.  Do this after the read
				// instead of before so the first read happens
				// promptly (so we avoid a long wait before the
				// first sample for sensors with large poll
				// intervals)
				sleep.await;
			}
		};

		let ioctx = SensorIOCtx {
			io,
			update_task: tokio::spawn(update_loop),
		};

		if s.io.replace(ioctx).is_some() {
			eprintln!("BUG: re-activating already-active sensor {}", s.name);
		}
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
	}
}

// Maps sensor names to Sensors
pub type SensorMap = HashMap<String, Arc<Mutex<Sensor>>>;
pub type SensorMapEntry<'a> = std::collections::hash_map::Entry<'a, String, Arc<Mutex<Sensor>>>;

fn read_from_sysfs<T: std::str::FromStr>(fd: &mut std::fs::File) -> ErrResult<T> {
	let mut s = String::new();

	// TODO: why does seem to call _llseek twice?  strace:
	//   [pid 21123] _llseek(23, 0, [0], SEEK_SET) = 0
	//   [pid 21123] _llseek(23, 0, [0], SEEK_CUR) = 0
	fd.rewind()?;

	// TODO: find alternative that calls read(2) once instead of
	// twice?  (this does one for data, plus another for EOF)
	fd.read_to_string(&mut s)?;

	// TODO: get .map_err() to work here?
	match s.trim().parse::<T>() {
		Ok(n) => Ok(n),
		Err(_) => Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData,
							   format!("invalid sysfs data: {}", s)))),
	}
}

// For cases where we expect exactly one .../hwmon/hwmonX subdirectory of a
// given path, this finds the one that's there, ensures there aren't any others,
// and returns it (including whatever original prefix was passed).  If no such
// hwmon directory is found, returns Ok(None); if multiple matches are found,
// return Err(...).
pub fn get_single_hwmon_dir(path: &str) -> ErrResult<Option<std::path::PathBuf>> {
	let pattern = format!("{}/hwmon/hwmon[0-9]*", path);
	let mut matches = glob::glob(&pattern)?;
	let first = match matches.next() {
		Some(m) => m?,
		None => return Ok(None),
	};
	if matches.next().is_some() {
		Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData,
						 "unexpected multiple hwmon directories?")))
	} else {
		Ok(Some(first))
	}
}

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

pub struct ValueIntfMsgFns {
	pub unit: Arc<PropChgMsgFn>,
	pub value: Arc<PropChgMsgFn>,
}

pub struct ValueIntfData {
	pub token: SensorIntfToken,
	pub msgfns: ValueIntfMsgFns,
}

fn build_sensor_value_intf(cr: &mut dbus_crossroads::Crossroads) -> ValueIntfData {
	let mut propchg_msgfns = None;
	let intf = "xyz.openbmc_project.Sensor.Value";
	let token = cr.register(intf, |b: &mut dbus_crossroads::IfaceBuilder<Arc<Mutex<Sensor>>>| {
		let unit_chgmsg = b.property("Unit")
			.get_async(|mut ctx, sensor| {
				let sensor = sensor.clone();
				async move {
					let k = sensor.lock().await.kind;
					ctx.reply(Ok(k.dbus_unit_str().to_string()))
				}
			})
			.emits_changed_true()
			.changed_msg_fn();

		let value_chgmsg = b.property("Value")
			.get_async(|mut ctx, sensor| {
				let sensor = sensor.clone();
				async move {
					let x = sensor.lock().await.cache.get();
					ctx.reply(Ok(x))
				}
			})
			.emits_changed_true()
			.changed_msg_fn();

		propchg_msgfns = Some(ValueIntfMsgFns {
			value: value_chgmsg.into(),
			unit: unit_chgmsg.into(),
		});
	});

	ValueIntfData {
		token,
		msgfns: propchg_msgfns.expect("no propchg_msgfns set?"),
	}
}

pub struct SensorIntfData {
	pub value: ValueIntfData,
	pub thresholds: HashMap<threshold::ThresholdSeverity, threshold::ThresholdIntfData>,
}

pub fn build_sensor_intfs(cr: &mut dbus_crossroads::Crossroads) -> SensorIntfData {
	SensorIntfData {
		value: build_sensor_value_intf(cr),
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
			filter: &FilterSet<dbus::Path<'static>>, i2cdevs: &Mutex<i2c::I2CDeviceMap>,
			conn: &Arc<SyncConnection>, intfs: &SensorIntfData) {
	let cfg = cfg.lock().await;
	let mut sensors = sensors.lock().await;

	adc::update_sensors(&cfg, &mut sensors, filter, conn, intfs).await.unwrap_or_else(|e| {
		eprintln!("Failed to update ADC sensors: {}", e);
	});

	let mut i2cdevs = i2cdevs.lock().await;
	hwmon::update_sensors(&cfg, &mut sensors, filter, &mut i2cdevs, intfs, conn).await.unwrap_or_else(|e| {
		eprintln!("Failed to update hwmon sensors: {}", e);
	});
}
