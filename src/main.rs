use std::{
	collections::{HashMap, HashSet},
	sync::Arc,
	time::Duration,
};
use dbus_tokio::connection;
use dbus::{
	channel::{MatchingReceiver, Sender},
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

mod types;
mod sensor;
mod adc;
mod hwmon;
mod i2c;
mod gpio;
mod powerstate;
mod threshold;

use types::*;
use sensor::{
	DBusSensor,
	DBusSensorMap,
	DBusSensorState,
	SensorConfig,
	SensorConfigMap,
};

const DBUS_NAME: &'static str = "xyz.openbmc_project.FooSensor";
const ENTITY_MANAGER_NAME: &'static str = "xyz.openbmc_project.EntityManager";

async fn get_config(bus: &nonblock::SyncConnection) -> ErrResult<SensorConfigMap> {
	let p = nonblock::Proxy::new(ENTITY_MANAGER_NAME, "/xyz/openbmc_project/inventory",
				     Duration::from_secs(30), bus);
	let objs = p.get_managed_objects().await?;
	let mut result = SensorConfigMap::new();

	'objloop: for (path, submap) in objs.into_iter() {
		println!("managed object: {}", path);
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
				"ADC" => {
					let cfg = match adc::ADCSensorConfig::from_dbus(props, k, &submap) {
						Some(c) => c,
						_ => {
							eprintln!("{}: malformed config data", path);
							continue;
						},
					};
					println!("\t{:?}", cfg);
					result.insert(Arc::new(path), SensorConfig::ADC(cfg));
					continue 'objloop;
				}
				"LM25066"|"W83773G"|"NCT6779" => {
					let cfg = match hwmon::HwmonSensorConfig::from_dbus(props, k, &submap) {
						Some(c) => c,
						_ => {
							eprintln!("{}: malformed config data", path);
							continue;
						},
					};
					println!("\t{:?}", cfg);
					result.insert(Arc::new(path), SensorConfig::Hwmon(cfg));
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

async fn deactivate_sensors(sensors: &mut DBusSensorMap) {
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

async fn register_properties_changed_handler<H, R>(bus: &nonblock::SyncConnection, cb: H) -> ErrResult<nonblock::MsgMatch>
	where H: Fn(dbus::message::Message, String, dbus::arg::PropMap) -> R + Send + Copy + Sync + 'static,
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

fn build_sensor_value_intf(cr: &mut Crossroads) -> (SensorIntfToken, PropChangeMsgFn) {
	let mut value_changed_msg_fn = None;
	let token = cr.register("xyz.openbmc_project.Sensor.Value", |b: &mut IfaceBuilder<Arc<Mutex<DBusSensor>>>| {
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
		let pb = b.property("Value")
			.get_async(|mut ctx, dbs| {
				let dbs = dbs.clone();
				async move {
					let x = match &dbs.lock().await.state {
						DBusSensorState::Active(s) => s.lock().await.cache,
						DBusSensorState::Phantom(_) => f64::NAN,
					};
					ctx.reply(Ok(x))
				}
			})
			.emits_changed_true();
		value_changed_msg_fn = Some(pb.changed_msg_fn());
	});
	(token, value_changed_msg_fn.expect("no value_changed_msg_fn set?"))
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

	let (value_intf, value_propchg_msgfn) = build_sensor_value_intf(&mut cr);
	let threshold_ifaces = threshold::build_sensor_threshold_intfs(&mut cr);
	powerstate::init_host_state(&sysbus).await;

	let send_value_propchg = move |dbuspath: &dbus::Path<'_>, old: f64, new: f64| {
		if old == new {
			return;
		}
		let msg = match value_propchg_msgfn(dbuspath,
						    &dbus::arg::Variant(new)) {
			Some(m) => m,
			None => {
				eprintln!("failed to create value-update message?");
				return;
			},
		};
		if sysbus.send(msg).is_err() {
			eprintln!("failed to send value-update propchange signal?");
		}
	};
	let send_value_propchg: SendValueChangeFn = Arc::new(Mutex::new(send_value_propchg));

	// FIXME (error handling)
	let cfg = get_config(&sysbus).await?;

	let mut sensors = HashMap::new();
	let mut i2cdevs = i2c::I2CDeviceMap::new();
	adc::update_sensors(&cfg, &mut sensors, send_value_propchg.clone(), &FilterSet::All).await?;
	hwmon::update_sensors(&cfg, &mut sensors, send_value_propchg.clone(), &FilterSet::All, &mut i2cdevs).await?;

	cr.insert("/xyz", &[], ());
	cr.insert("/xyz/openbmc_project", &[], ());
	cr.insert("/xyz/openbmc_project/sensors", &[cr.object_manager()], ());

	let badchar = |c: char| !(c.is_ascii_alphanumeric() || c == '_');

	for (name, dbs) in &sensors {
		let inner = dbs.lock().await;
		let s = match inner.state {
			DBusSensorState::Active(ref s) => s.lock().await,
			DBusSensorState::Phantom(_) => {
				eprintln!("{}: phantom sensor during initialization?", name);
				continue;
			},
		};
		let cleanname = s.name.replace(badchar, "_");
		let dbuspath = format!("/xyz/openbmc_project/sensors/{}/{}", s.kind.dbus_category(), cleanname);
		let mut ifaces = vec![value_intf];
		for t in s.thresholds.keys() {
			ifaces.push(*threshold_ifaces.get(t).expect("no interface for threshold severity"));
		}
		cr.insert(dbuspath, &ifaces, dbs.clone());
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

	let send_value_propchg: &_ = Box::leak(Box::new(send_value_propchg));
	let powerhandler = move |kind, newstate| {
		async move {
			eprintln!("got a power signal: {:?} -> {}", kind, newstate);
			let mut sensors = refsensors.lock().await;
			let cfg = refcfg.lock().await;

			if newstate {
				adc::update_sensors(&cfg, &mut sensors, send_value_propchg.clone(), &FilterSet::All).await
					.unwrap_or_else(|e| {
						eprintln!("ADC sensor update failed: {}", e);
					});

				let mut i2cdevs = refi2cdevs.lock().await;

				hwmon::update_sensors(&cfg, &mut sensors, send_value_propchg.clone(), &FilterSet::All, &mut i2cdevs).await
					.unwrap_or_else(|e| {
						eprintln!("Hwmon sensor update failed: {}", e);
					});
			} else {
				deactivate_sensors(&mut sensors).await;
			}
			eprintln!("[{:?}] signal handler done.", kind);
		}
	};

	let _powersignals = powerstate::register_power_signal_handler(&sysbus, powerhandler).await?;

	#[allow(non_upper_case_globals)]
	static changed_paths: Mutex<Option<HashSet<dbus::Path>>> = Mutex::const_new(None);

	let prophandler = move |msg: dbus::message::Message, _, _| {
		async move {
			let path = match msg.path() {
				Some(p) => p.into_static(),
				_ => return,
			};

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

			tokio::time::sleep(Duration::from_secs(2)).await;

			let mut paths = changed_paths.lock().await;
			let mut sensors = refsensors.lock().await;
			let mut cfg = refcfg.lock().await;

			let filter: FilterSet<_> = if paths.is_some() {
				paths.take().into()
			} else {
				// This should be impossible, but try to lessen the
				// impact if it somehow happens...
				eprintln!("BUG: changed_paths vanished out from under us!");
				return;
			};

			*cfg = match get_config(&sysbus).await {
				Ok(c) => c,
				Err(e) => {
					eprintln!("Failed to retrieve sensor configs, ignoring PropertiesChanged: {}", e);
					return;
				},
			};

			// FIXME: dedupe w/ similar code in powerhandler
			adc::update_sensors(&cfg, &mut sensors, send_value_propchg.clone(), &filter).await
				.unwrap_or_else(|e| {
					eprintln!("ADC sensor update failed: {}", e);
				});

			let mut i2cdevs = refi2cdevs.lock().await;

			hwmon::update_sensors(&cfg, &mut sensors, send_value_propchg.clone(), &filter, &mut i2cdevs).await
				.unwrap_or_else(|e| {
					eprintln!("Hwmon sensor update failed: {}", e);
				});
		}
	};

	let _propsignals = register_properties_changed_handler(&sysbus, prophandler).await?;

	println!("Hello, world!");

	future::pending::<()>().await;
	unreachable!();
}
