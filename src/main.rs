use std::{
	collections::HashMap,
	sync::Arc,
	time::Duration,
};
use dbus_tokio::connection;
use dbus::{
	channel::MatchingReceiver,
	message::MatchRule,
	nonblock,
	nonblock::stdintf::org_freedesktop_dbus::ObjectManager,
};
use dbus_crossroads::{
	Crossroads,
	IfaceBuilder,
};
use futures::future;
use tokio::sync::Mutex;

mod sensor;
mod adc;
mod hwmon;
mod i2c;
mod gpio;
mod powerstate;

use sensor::{
	DBusSensor,
	DBusSensorMap,
	SensorConfig,
	SensorConfigMap,
};

pub type ErrResult<T> = Result<T, Box<dyn std::error::Error>>;

const DBUS_NAME: &'static str = "xyz.openbmc_project.FooSensor";
const ENTITY_MANAGER_NAME: &'static str = "xyz.openbmc_project.EntityManager";

async fn get_config(bus: &nonblock::SyncConnection) -> ErrResult<SensorConfigMap<'_>> {
	let p = nonblock::Proxy::new(ENTITY_MANAGER_NAME, "/xyz/openbmc_project/inventory",
				     Duration::from_secs(30), bus);
	let objs = p.get_managed_objects().await?;
	let mut result = SensorConfigMap::new();

	for (path, submap) in &objs {
		println!("managed object: {}", path);
		for (k, props) in submap {
			let parts: Vec<&str> = k.split('.').collect();
			if parts.len() != 4
				|| parts[0] != "xyz"
				|| parts[1] != "openbmc_project"
				|| parts[2] != "Configuration" {
				continue
			}
			let cfgtype = parts[3];

			match cfgtype {
				"ADC" => {
					let cfg = match adc::ADCSensorConfig::from_dbus(props, submap) {
						Some(c) => c,
						_ => {
							eprintln!("{}: malformed config data", path);
							continue;
						},
					};
					println!("\t{:?}", cfg);
					result.insert(path.clone(), Arc::new(SensorConfig::ADC(cfg)));
				}
				"LM25066"|"W83773G"|"NCT6779" => {
					let cfg = match hwmon::HwmonSensorConfig::from_dbus(props, submap) {
						Some(c) => c,
						_ => {
							eprintln!("{}: malformed config data", path);
							continue;
						},
					};
					println!("\t{:?}", cfg);
					result.insert(path.clone(), Arc::new(SensorConfig::Hwmon(cfg)));
				},
				_ => {
					println!("\t{}:", k);
					for (p, v) in props {
						println!("\t\t{}: {:?}", p, v);
					}
				}
			}
		}
	}
	Ok(result)
}

async fn deactivate_sensors(sensors: &mut DBusSensorMap) {
	for (name, dbs) in sensors.iter_mut() {
		let dbs = &mut *dbs.lock().await;
		let new = {
			let arcsensor = match dbs {
				DBusSensor::Active(s) => s,
				DBusSensor::Phantom(_) => continue,
			};
			let sensor = arcsensor.lock().await;
			if sensor.active_now().await {
				continue;
			}
			let nrefs = Arc::strong_count(&arcsensor);
			if nrefs > 1 {
				eprintln!("{} refcount = {} at deactivation, will leak!", name, nrefs);
			}
			sensor.deactivate()
		};
		dbs.replace(new);
	}
}

#[tokio::main]
async fn main() -> ErrResult<()> {
	let (sysbus_resource, sysbus) = connection::new_system_sync()?;
	let _handle = tokio::spawn(async {
		let err = sysbus_resource.await;
		panic!("Lost connection to D-Bus: {}", err);
	});

	// HACK: this is effectively a global; leak it so we can
	// borrow from it for other things that are leaked later on
	// (see below)
	let sysbus: &_ = Box::leak(Box::new(sysbus));

	sysbus.request_name(DBUS_NAME, false, false, false).await?;

	let mut cr = Crossroads::new();
	cr.set_async_support(Some((sysbus.clone(), Box::new(|x| { tokio::spawn(x); }))));
	cr.set_object_manager_support(Some(sysbus.clone()));

	let sensor_value_iface = cr.register("xyz.openbmc_project.Sensor.Value",
					     |b: &mut IfaceBuilder<Arc<Mutex<DBusSensor>>>| {
		b.property("Unit")
			.get_async(|mut ctx, dbs| {
				let dbs = dbs.clone();
				async move {
					let u = dbs.lock().await
						.kind().await
						.dbus_unit_str();
					ctx.reply(Ok(u.to_string()))
				}
			});
		b.property("Value")
			.get_async(|mut ctx, dbs| {
				let dbs = dbs.clone();
				async move {
					let x = match &*dbs.lock().await {
						DBusSensor::Active(s) => s.lock().await.cache,
						DBusSensor::Phantom(_) => f64::NAN,
					};
					ctx.reply(Ok(x))
				}
			});
	});

	powerstate::init_host_state(&sysbus).await;

	// FIXME (error handling)
	let cfg = get_config(&sysbus).await?;

	let mut sensors = HashMap::new();
	let mut i2cdevs = i2c::I2CDeviceMap::new();
	adc::update_adc_sensors(&cfg, &mut sensors).await?;
	hwmon::update_hwmon_sensors(&cfg, &mut sensors, &mut i2cdevs).await?;

	cr.insert("/xyz", &[], ());
	cr.insert("/xyz/openbmc_project", &[], ());
	cr.insert("/xyz/openbmc_project/sensors", &[cr.object_manager()], ());

	let badchar = |c: char| !(c.is_ascii_alphanumeric() || c == '_');

	for (name, dbs) in &sensors {
		let inner = dbs.lock().await;
		let s = match *inner {
			DBusSensor::Active(ref s) => s.lock().await,
			DBusSensor::Phantom(_) => {
				eprintln!("{}: phantom sensor during initialization?", name);
				continue;
			},
		};
		let cleanname = s.name.replace(badchar, "_");
		let dbuspath = format!("/xyz/openbmc_project/sensors/{}/{}", s.kind.dbus_category(), cleanname);
		cr.insert(dbuspath, &[sensor_value_iface], dbs.clone());
	}

	sysbus.start_receive(MatchRule::new_method_call(), Box::new(move |msg, conn| {
		cr.handle_message(msg, conn).expect("wtf?");
		true
	}));

	fn globalize<T>(x: T) -> &'static Mutex<T> {
		Box::leak(Box::new(Mutex::new(x)))
	}
	// HACK: leak these into a pseudo-globals (to satisfy callback
	// lifetime requirements).  Once const HashMap::new() is
	// stable we can switch these to be real globals instead.
	let refsensors = globalize(sensors);
	let refcfg = globalize(cfg);
	let refi2cdevs = globalize(i2cdevs);

	let handler = move |kind, newstate: bool| async move {
		eprintln!("got a power signal: {:?} -> {}", kind, newstate);
		let mut sensors = refsensors.lock().await;
		let cfg = refcfg.lock().await;

		if newstate {
			adc::update_adc_sensors(&cfg, &mut sensors).await
				.unwrap_or_else(|e| {
					eprintln!("ADC sensor update failed: {}", e);
				});

			let mut i2cdevs = refi2cdevs.lock().await;

			// FIXME: dedupe error handling w/ above?
			hwmon::update_hwmon_sensors(&cfg, &mut sensors, &mut i2cdevs).await
				.unwrap_or_else(|e| {
					eprintln!("Hwmon sensor update failed: {}", e);
				});
		} else {
			deactivate_sensors(&mut sensors).await;
		}
		eprintln!("[{:?}] signal handler done.", kind);
	};

	let _signals = powerstate::register_power_signal_handler(&sysbus, handler).await?;

	println!("Hello, world!");

	future::pending::<()>().await;
	unreachable!();
}
