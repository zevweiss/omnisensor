use std::{
	collections::HashMap,
	io::{Read, Seek},
	ops::DerefMut,
	sync::Arc,
	time::Duration,
};
use tokio::sync::Mutex;
use glob;

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

pub struct Sensor {
	pub name: String,
	pub kind: SensorType,
	cache: AutoProp<f64>,

	fd: std::fs::File,
	bridge_gpio: Option<BridgeGPIO>,
	i2cdev: Option<Arc<I2CDevice>>,
	poll_interval: Duration,
	power_state: PowerState,

	pub thresholds: Thresholds,

	// This is a combined scale factor set to the product of the
	// innate hwmon scaling factor (e.g. 1000 to convert a sysfs
	// millivolt value to volts) and any optional additional
	// scaling factor specified in the sensor config (e.g. to
	// translate the raw value of a voltage-divided ADC line back
	// to its "real" 12V range or the like).
	scale: f64,

	update_task: Option<tokio::task::JoinHandle<()>>,
}

impl Sensor {
	pub fn new(name: &str, kind: SensorType, fd: std::fs::File) -> Self {
		Self {
			name: name.into(),
			kind,
			fd,
			cache: AutoProp::new(f64::NAN),
			bridge_gpio: None,
			i2cdev: None,
			poll_interval: Duration::from_secs(1),
			power_state: PowerState::Always,
			thresholds: Thresholds::new(),
			scale: kind.hwmon_scale(),
			update_task: None,
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

	async fn active_now(&self) -> bool {
		self.power_state.active_now().await
	}

	async fn set_value(&mut self, newval: f64) {
		self.cache.set(newval);

		for t in self.thresholds.values_mut() {
			t.update(newval);
		}
	}

	async fn update(&mut self) -> ErrResult<()> {
		let ival = {
			let _gpio_hold = match self.bridge_gpio.as_ref().map(|g| g.activate()) {
				Some(x) => match x.await {
					Ok(h) => Some(h),
					Err(e) => {
						return Err(e);
					},
				},
				None => None,
			};

			read_from_sysfs::<i32>(&mut self.fd)?
		};

		self.set_value((ival as f64) * self.scale).await;
		Ok(())
	}

	async fn start_updates(self) -> Arc<Mutex<Self>> {
		let mut timer = tokio::time::interval(self.poll_interval);
		let s = Arc::new(Mutex::new(self));

		// Use a weak reference in the update task closure so
		// it doesn't hold a strong reference to the sensor
		// (which would create a reference loop via
		// s.update_task and make it un-droppable)
		let w = Arc::downgrade(&s);

		let update_loop = async move {
			loop {
				if let Some(t) = w.upgrade() {
					let mut sensor = t.lock().await;
					if let Err(e) = sensor.update().await {
						eprintln!("failed to update {}: {}", sensor.name, e);
					}
				}

				// wait after read instead of before so the
				// first one happens promptly (so we avoid a
				// long wait before the first sample for sensors
				// with large poll intervals)
				timer.tick().await;
			}
		};
		{
			let mut r = s.lock().await;
			let r = r.deref_mut();
			r.update_task = Some(tokio::spawn(update_loop));
		}
		s
	}

	// FIXME: wanted this to consume self, not take by reference, but can't
	// consume/replace it inside a Mutex/MutexGuard AFAICT...
	fn deactivate(&mut self) -> DBusSensorState {
		DBusSensorState::Phantom(PhantomSensor {
			kind: self.kind,
			thresholds: std::mem::take(&mut self.thresholds),
		})
	}

	fn arm_autoprops(&mut self, conn: &Arc<dbus::nonblock::SyncConnection>, dbuspath: &Arc<dbus::Path<'static>>, intfs: &SensorIntfData) {
		self.cache.arm(conn, dbuspath, &intfs.value.msgfns.value);
		for (sev, threshold) in self.thresholds.iter_mut() {
			let threshold_intf = match intfs.thresholds.get(sev) {
				Some(i) => i,
				_ => {
					eprintln!("no interface found for {:?}?", sev);
					continue;
				},
			};

			threshold.arm_autoprops(conn, dbuspath, &threshold_intf.msgfns)
		}
	}
}

impl Drop for Sensor {
	fn drop(&mut self) {
		if let Some(tsk) = &self.update_task {
			tsk.abort();
		}
	}
}

// Data that stays behind after the underlying sensor goes away so we
// can continue serving it up over dbus
pub struct PhantomSensor {
	kind: SensorType,
	pub thresholds: Thresholds,
}

impl PhantomSensor {
	// FIXME: wanted this to consume self, not take by reference, but can't
	// consume/replace it inside a Mutex/MutexGuard AFAICT...
	async fn activate(&self, path: Arc<dbus::Path<'static>>, sensor: Sensor) -> DBusSensor {
		if sensor.kind != self.kind {
			eprintln!("{}: sensor type changed on activation? ({:?} -> {:?})",
				  sensor.name, self.kind, sensor.kind);
		}
		DBusSensor::new(path.clone(), sensor).await
	}
}

// A Sensor wrapper that persists for dbus purposes even if the
// underlying ("real") sensor goes away (e.g. if it's PowerState::On
// and the host gets shut off).
pub enum DBusSensorState {
	Active(Arc<Mutex<Sensor>>),
	Phantom(PhantomSensor),
}

pub struct DBusSensor {
	dbuspath: Arc<dbus::Path<'static>>,
	pub state: DBusSensorState,
}

// Maps sensor names to DBusSensors
pub type DBusSensorMap = HashMap<String, Arc<Mutex<DBusSensor>>>;
type DBusSensorMapEntry<'a> = std::collections::hash_map::Entry<'a, String, Arc<Mutex<DBusSensor>>>;

impl DBusSensor {
	async fn new(dbuspath: Arc<dbus::Path<'static>>, sensor: Sensor) -> Self {
		Self {
			dbuspath: dbuspath.clone(),
			state: DBusSensorState::Active(sensor.start_updates().await),
		}
	}

	fn update_state(&mut self, new: DBusSensorState) {
		drop(std::mem::replace(&mut self.state, new))
	}

	async fn kind(&self) -> SensorType {
		match &self.state {
			DBusSensorState::Active(s) => s.lock().await.kind,
			DBusSensorState::Phantom(p) => p.kind,
		}
	}

	pub async fn arm_autoprops(&mut self, conn: &Arc<dbus::nonblock::SyncConnection>, intfs: &SensorIntfData) {
		match &self.state {
			DBusSensorState::Active(s) => {
				 let mut s = s.lock().await;
				s.arm_autoprops(conn, &self.dbuspath, intfs);
			},
			_ => {
				// FIXME!  what happens to AutoProps when things go phantom?
			},
		}
	}
}

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

pub async fn get_nonactive_sensor_entry(sensors: &mut DBusSensorMap, key: String) -> Option<DBusSensorMapEntry<'_>>
{
	let entry = sensors.entry(key);
	if let DBusSensorMapEntry::Occupied(ref e) = entry {
		if let DBusSensorState::Active(_) = e.get().lock().await.state {
			return None;
		}
	}
	Some(entry)
}

// Install a sensor into a given hashmap entry, returning None if the entry was
// already occupied by an active sensor and Some(()) otherwise
pub async fn install_sensor(mut entry: DBusSensorMapEntry<'_>, dbuspath: Arc<dbus::Path<'static>>,
			    sensor: Sensor) -> Option<()>
{
	match entry {
		DBusSensorMapEntry::Vacant(e) => {
			e.insert(Arc::new(Mutex::new(DBusSensor::new(dbuspath.clone(), sensor).await)));
		},
		DBusSensorMapEntry::Occupied(ref mut e) => {
			let new = {
				let dbs = e.get().lock().await;
				match &dbs.state {
					DBusSensorState::Phantom(p) => Arc::new(Mutex::new(p.activate(dbuspath.clone(), sensor).await)),
					DBusSensorState::Active(_) =>  return None,
				}
			};
			e.insert(new);
		},
	};

	Some(())
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
	let token = cr.register(intf, |b: &mut dbus_crossroads::IfaceBuilder<Arc<Mutex<DBusSensor>>>| {
		let unit_chgmsg = b.property("Unit")
			.get_async(|mut ctx, dbs| {
				let dbs = dbs.clone();
				async move {
					let u = dbs.lock().await
						.kind().await
						.dbus_unit_str();
					ctx.reply(Ok(u.to_string()))
				}
			})
			.emits_changed_true()
			.changed_msg_fn();

		let value_chgmsg = b.property("Value")
			.get_async(|mut ctx, dbs| {
				let dbs = dbs.clone();
				async move {
					let x = match &dbs.lock().await.state {
						DBusSensorState::Active(s) => s.lock().await.cache.get(),
						DBusSensorState::Phantom(_) => f64::NAN,
					};
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

pub async fn deactivate(sensors: &mut DBusSensorMap) {
	for (name, dbs) in sensors.iter_mut() {
		let dbs = &mut *dbs.lock().await;
		let new = {
			let arcsensor = match &dbs.state {
				DBusSensorState::Active(s) => s,
				DBusSensorState::Phantom(_) => continue,
			};
			let mut sensor = arcsensor.lock().await;
			if sensor.active_now().await {
				continue;
			}
			let nrefs = Arc::strong_count(&arcsensor);
			if nrefs > 1 {
				eprintln!("{} refcount = {} at deactivation, will leak!", name, nrefs);
			}
			sensor.deactivate()
		};
		dbs.update_state(new);
	}
}

pub async fn update_all(cfg: &Mutex<SensorConfigMap>, sensors: &Mutex<DBusSensorMap>,
			filter: &FilterSet<dbus::Path<'static>>, i2cdevs: &Mutex<i2c::I2CDeviceMap>) {
	let cfg = cfg.lock().await;
	let mut sensors = sensors.lock().await;

	adc::update_sensors(&cfg, &mut sensors, filter).await.unwrap_or_else(|e| {
		eprintln!("Failed to update ADC sensors: {}", e);
	});

	let mut i2cdevs = i2cdevs.lock().await;
	hwmon::update_sensors(&cfg, &mut sensors, filter, &mut i2cdevs).await.unwrap_or_else(|e| {
		eprintln!("Failed to update hwmon sensors: {}", e);
	});
}
