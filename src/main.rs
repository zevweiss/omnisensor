//! omnisensor: a unified OpenBMC sensor daemon.
//!
//! Configuration data retrieved from entity-manager is used to
//! instantiate sensor objects that present readings and other data
//! (units, thresholds, operational status, etc.) via dbus.

use std::{
	collections::HashSet,
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
		stdintf::org_freedesktop_dbus::{
			ObjectManager,
			RequestNameReply,
		},
	},
};
use dbus_crossroads::Crossroads;
use futures::future;
use tokio::sync::Mutex;

mod types;
mod sensor;
#[cfg(feature = "adc")]
mod adc;
#[cfg(feature = "fan")]
mod fan;
#[cfg(feature = "hwmon")]
mod hwmon;
#[cfg(feature = "peci")]
mod peci;
#[cfg(feature = "external")]
mod external;
mod i2c;
mod gpio;
mod powerstate;
mod sysfs;
mod threshold;
mod dbus_helpers;

#[cfg(feature = "hostpower")]
use powerstate::host_state;
use types::*;
use sensor::{
	SensorConfig,
	SensorConfigMap,
	SensorIntfData,
	SensorMap,
};

/// The dbus name claimed by the daemon.
const DBUS_NAME: &str = "xyz.openbmc_project.OmniSensor";

/// The dbus service from which we retrieve config data.
const ENTITY_MANAGER_NAME: &str = "xyz.openbmc_project.EntityManager";

/// Retrieve sensor config data from entity-manager via dbus.
async fn get_config(bus: &SyncConnection) -> ErrResult<SensorConfigMap> {
	let get_objects = |path| async move {
		let p = nonblock::Proxy::new(ENTITY_MANAGER_NAME, path,
					     Duration::from_secs(30), bus);
		p.get_managed_objects().await
	};

	// Try the new ObjectManager location first, but fall back to the old one in case that fails
	let objs = match get_objects("/xyz/openbmc_project/inventory").await {
		Ok(objs) => objs,
		Err(e) => {
			// ...but if both fail, return the error from the first one.
			match get_objects("/").await {
				Ok(objs) => objs,
				Err(_) => return Err(e.into()),
			}
		},
	};

	let mut result = SensorConfigMap::new();

	'objloop: for (path, submap) in objs.into_iter().map(|(p, s)| (InventoryPath(p.clone()), s)) {
		for (k, props) in &submap {
			let Some(cfg) = SensorConfig::from_dbus(props, k, &submap) else {
				continue;
			};
			let cfg = match cfg {
				Ok(cfg) => cfg,
				Err(e) => {
					eprintln!("{:?}, {}: {}", path, k, e);
					continue;
				},
			};
			result.insert(Arc::new(path), cfg);
			continue 'objloop;
		}
	}
	Ok(result)
}

/// Arranges for `cb` to be called on receipt of any `PropertiesChanged` dbus
/// signals.
///
/// `cb` is passed the signal message, the dbus interface to which it pertains,
/// and the associated properties.
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

/// Global daemon-wide state
pub struct DaemonState {
	/// Our collected, parsed (structured) sensor config data.
	config: Mutex<SensorConfigMap>,
	/// All extant sensors, by name (active and inactive alike).
	sensors: Mutex<SensorMap>,
	/// All managed (dynamic) I2C devices.
	i2cdevs: Mutex<i2c::I2CDeviceMap>,
	/// Our dbus connection.
	bus: Arc<SyncConnection>,
	/// dbus object server registry...thing.
	crossroads: SyncMutex<dbus_crossroads::Crossroads>,
	/// Sensor dbus interface metadata.
	sensor_intfs: SensorIntfData,
}

/// A helper function for handling dbus `PropertiesChanged` signals.
async fn handle_propchange(daemonstate: &DaemonState, msg: dbus::message::Message) {
	#[allow(non_upper_case_globals)]
	static changed_paths: Mutex<Option<HashSet<InventoryPath>>> = Mutex::const_new(None);

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

	let newcfg = match get_config(&daemonstate.bus).await {
		Ok(c) => c,
		Err(e) => {
			eprintln!("Failed to retrieve sensor configs, ignoring PropertiesChanged: {}", e);
			return;
		},
	};

	{
		*daemonstate.config.lock().await = newcfg;
	}

	sensor::instantiate_all(daemonstate, &filter).await;
}

/// The entry point of the daemon.
#[tokio::main]
async fn main() -> ErrResult<()> {
	let (bus_resource, bus) = connection::new_system_sync()?;
	let _handle = tokio::spawn(async {
		let err = bus_resource.await;
		panic!("Lost connection to D-Bus: {}", err);
	});

	let mut cr = Crossroads::new();
	cr.set_async_support(Some((bus.clone(), Box::new(|x| { tokio::spawn(x); }))));
	cr.set_object_manager_support(Some(bus.clone()));

	let sensor_intfs = sensor::SensorIntfData::build(&mut cr);

	#[cfg(feature = "hostpower")]
	host_state::init_host_state(&bus).await;

	let cfg = get_config(&bus).await?; // FIXME (error handling)

	cr.insert("/xyz", &[], ());
	cr.insert("/xyz/openbmc_project", &[], ());
	let objmgr = cr.object_manager();
	cr.insert("/xyz/openbmc_project/sensors", &[objmgr], ());

	let daemonstate = DaemonState {
		config: Mutex::new(cfg),
		sensors: Mutex::new(SensorMap::new()),
		i2cdevs: Mutex::new(i2c::I2CDeviceMap::new()),
		bus,
		crossroads: SyncMutex::new(cr),
		sensor_intfs,
	};

	// HACK: leak this into a pseudo-global to satisfy callback lifetime requirements
	// (Arc-ing it would be sort of silly; its lifetime is the program's lifetime).
	let daemonstate: &_ = Box::leak(Box::new(daemonstate));

	sensor::instantiate_all(daemonstate, &FilterSet::All).await;

	#[cfg(feature = "hostpower")]
	let _powersignals = {
		let powerhandler = move |_kind, newstate| async move {
			if newstate {
				sensor::instantiate_all(daemonstate, &FilterSet::All).await;
			} else {
				let mut sensors = daemonstate.sensors.lock().await;
				sensor::deactivate(&mut sensors).await;
			}
		};
		host_state::register_power_signal_handler(&daemonstate.bus, powerhandler).await?
	};

	let prophandler = move |msg: dbus::message::Message, _, _| async move {
		handle_propchange(daemonstate, msg).await;
	};

	let _propsignals = register_properties_changed_handler(&daemonstate.bus, prophandler).await?;

	daemonstate.bus.start_receive(MatchRule::new_method_call(), Box::new(move |msg, conn| {
		let mut cr = daemonstate.crossroads.lock().unwrap();
		cr.handle_message(msg, conn).expect("wtf?");
		true
	}));

	let reply = daemonstate.bus.request_name(DBUS_NAME, false, false, true).await?;
	match reply {
		RequestNameReply::PrimaryOwner => (), // OK
		_ => {
			let msg = format!("Failed to acquire dbus name {}: {:?}", DBUS_NAME, reply);
			return Err(err_other(msg));
		},
	}

	println!("Hello, world!");

	future::pending::<()>().await;
	unreachable!();
}
