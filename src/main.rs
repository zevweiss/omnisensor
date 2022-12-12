use std::{
	collections::{HashMap, HashSet},
	sync::{Arc, Mutex as SyncMutex},
	time::Duration,
};
use dbus_tokio::connection;
use dbus::{
	channel::MatchingReceiver,
	message::MatchRule,
	nonblock,
	nonblock::{
		SyncConnection,
		stdintf::org_freedesktop_dbus::ObjectManager,
	},
};
use dbus_crossroads::Crossroads;
use futures::future;
use tokio::sync::Mutex;

mod types;
mod sensor;
#[cfg(feature = "adc")]
mod adc;
mod hwmon;
#[cfg(feature = "peci")]
mod peci;
mod i2c;
mod gpio;
mod powerstate;
mod sysfs;
mod threshold;
mod dbus_helpers;

use types::*;
use sensor::{
	SensorConfig,
	SensorConfigMap,
	SensorIntfData,
	SensorMap,
};

const DBUS_NAME: &str = "xyz.openbmc_project.OmniSensor";
const ENTITY_MANAGER_NAME: &str = "xyz.openbmc_project.EntityManager";

async fn get_config(bus: &SyncConnection) -> ErrResult<SensorConfigMap> {
	let p = nonblock::Proxy::new(ENTITY_MANAGER_NAME, "/xyz/openbmc_project/inventory",
				     Duration::from_secs(30), bus);
	let objs = p.get_managed_objects().await?;
	let mut result = SensorConfigMap::new();

	'objloop: for (path, submap) in objs.into_iter().map(|(p, s)| (InventoryPath(p.clone()), s)) {
		println!("managed object: {}", path.0);
		for (k, props) in &submap {
			let parts: Vec<&str> = k.split('.').collect();
			if parts.len() != 4
				|| parts[0] != "xyz"
				|| parts[1] != "openbmc_project"
				|| parts[2] != "Configuration" {
				continue
			}
			let cfgtype = parts[3];

			match cfgtype {
				#[cfg(feature = "adc")]
				"ADC" => {
					let cfg = match adc::ADCSensorConfig::from_dbus(props, k, &submap) {
						Ok(c) => c,
						Err(e) => {
							eprintln!("{}: malformed config data: {}", path.0, e);
							continue;
						},
					};
					println!("\t{:?}", cfg);
					result.insert(Arc::new(path), SensorConfig::ADC(cfg));
					continue 'objloop;
				}

				"LM25066"|"W83773G"|"NCT6779" => {
					let cfg = match hwmon::HwmonSensorConfig::from_dbus(props, k, &submap) {
						Ok(c) => c,
						Err(e) => {
							eprintln!("{}: malformed config data: {}", path.0, e);
							continue;
						},
					};
					println!("\t{:?}", cfg);
					result.insert(Arc::new(path), SensorConfig::Hwmon(cfg));
					continue 'objloop;
				},

				#[cfg(feature = "peci")]
				"XeonCPU" => {
					let cfg = match peci::PECISensorConfig::from_dbus(props, k, &submap) {
						Ok(c) => c,
						Err(e) => {
							eprintln!("{}: malformed config data: {}", path.0, e);
							continue;
						},
					};
					println!("\t{:?}", cfg);
					result.insert(Arc::new(path), SensorConfig::PECI(cfg));
					continue 'objloop;
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

async fn register_properties_changed_handler<H, R>(bus: &SyncConnection, cb: H) -> ErrResult<nonblock::MsgMatch>
	where H: FnOnce(dbus::message::Message, String, dbus::arg::PropMap) -> R + Send + Copy + Sync + 'static,
	      R: futures::Future<Output = ()> + Send
{
	use dbus::message::SignalArgs;
	use nonblock::stdintf::org_freedesktop_dbus::PropertiesPropertiesChanged as PPC;
	use futures::StreamExt;

	let rule = MatchRule::new_signal(PPC::INTERFACE, PPC::NAME)
		.with_namespaced_path("/xyz/openbmc_project/inventory");
	let (signal, stream) = bus.add_match(rule).await?.stream();
	let stream = stream.for_each(move |(msg, (intf, props)): (_, (String, dbus::arg::PropMap))| async move {
		// until dbus-rs supports arg0namespace as a MatchRule
		// parameter, do it manually here...
		if !intf.starts_with("xyz.openbmc_project.Configuration.") {
			return;
		}
		tokio::spawn(async move { cb(msg, intf, props).await });
	});

	tokio::spawn(async { stream.await });

	Ok(signal)
}

async fn handle_propchange(bus: &Arc<SyncConnection>, cfg: &Mutex<SensorConfigMap>, sensors: &Mutex<SensorMap>,
			   i2cdevs: &Mutex<i2c::I2CDeviceMap>, changed_paths: &Mutex<Option<HashSet<InventoryPath>>>,
			   cr: &SyncMutex<dbus_crossroads::Crossroads>, msg: dbus::message::Message, sensor_intfs: &SensorIntfData) {
	let Some(path) = msg.path().map(|p| p.into_static()) else {
		return;
	};
	let path = InventoryPath(path);

	{
		let mut paths = changed_paths.lock().await;
		if let Some(ref mut set) = &mut *paths {
			// if it was already Some(_), piggyback on the
			// signal that set it that way
			set.insert(path);
			return;
		}

		*paths = Some([path].into_iter().collect());
	}

	// Wait for other signals to arrive so we can handle them all as a batch
	tokio::time::sleep(Duration::from_secs(2)).await;

	let mut paths = changed_paths.lock().await;

	let filter: FilterSet<_> = if paths.is_some() {
		paths.take().into()
	} else {
		// This should be impossible, but try to lessen the
		// impact if it somehow happens (instead of .expect()-ing)
		eprintln!("BUG: changed_paths vanished out from under us!");
		return;
	};

	let newcfg = match get_config(bus).await {
		Ok(c) => c,
		Err(e) => {
			eprintln!("Failed to retrieve sensor configs, ignoring PropertiesChanged: {}", e);
			return;
		},
	};

	{
		*cfg.lock().await = newcfg;
	}

	sensor::update_all(cfg, sensors, &filter, i2cdevs, cr, bus, sensor_intfs).await;
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

	fn globalize<T>(x: T) -> &'static Mutex<T> {
		Box::leak(Box::new(Mutex::new(x)))
	}

	let cr: &_ = Box::leak(Box::new(SyncMutex::new(Crossroads::new())));
	let mut crlock = cr.lock().unwrap();
	crlock.set_async_support(Some((sysbus.clone(), Box::new(|x| { tokio::spawn(x); }))));
	crlock.set_object_manager_support(Some(sysbus.clone()));

	let sensor_intfs: &_ = Box::leak(Box::new(sensor::build_sensor_intfs(&mut crlock)));

	powerstate::init_host_state(sysbus).await;

	// HACK: leak things into a pseudo-globals (to satisfy
	// callback lifetime requirements).  Once const HashMap::new()
	// is stable we can switch these to be real globals instead.
	let sensors = globalize(HashMap::new());
	let i2cdevs = globalize(i2c::I2CDeviceMap::new());
	let cfg = globalize(get_config(sysbus).await?); // FIXME (error handling)

	crlock.insert("/xyz", &[], ());
	crlock.insert("/xyz/openbmc_project", &[], ());
	let objmgr = crlock.object_manager();
	crlock.insert("/xyz/openbmc_project/sensors", &[objmgr], ());

	drop(crlock);

	sensor::update_all(cfg, sensors, &FilterSet::All, i2cdevs, &cr, sysbus, sensor_intfs).await;

	let powerhandler = move |_kind, newstate| async move {
		if newstate {
			sensor::update_all(cfg, sensors, &FilterSet::All, i2cdevs, &cr, sysbus, sensor_intfs).await;
		} else {
			let mut sensors = sensors.lock().await;
			sensor::deactivate(&mut sensors).await;
		}
	};

	let _powersignals = powerstate::register_power_signal_handler(sysbus, powerhandler).await?;

	#[allow(non_upper_case_globals)]
	static changed_paths: Mutex<Option<HashSet<InventoryPath>>> = Mutex::const_new(None);

	let prophandler = move |msg: dbus::message::Message, _, _| async move {
		handle_propchange(sysbus, cfg, sensors, i2cdevs, &changed_paths, &cr, msg, sensor_intfs).await;
	};

	let _propsignals = register_properties_changed_handler(sysbus, prophandler).await?;

	sysbus.start_receive(MatchRule::new_method_call(), Box::new(move |msg, conn| {
		let mut cr = cr.lock().unwrap();
		cr.handle_message(msg, conn).expect("wtf?");
		true
	}));

	println!("Hello, world!");

	future::pending::<()>().await;
	unreachable!();
}
