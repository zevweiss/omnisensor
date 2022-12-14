//! Utility code for monitoring host power state transitions.

use std::{
	ops::DerefMut,
	sync::Mutex as SyncMutex,
};
use dbus::{
	message::MatchRule,
	nonblock,
	nonblock::stdintf::org_freedesktop_dbus::Properties,
};

use crate::types::*;

/// Represents a setting of a sensor indicating in which host power states it is enabled.
#[derive(Debug, Copy, Clone)]
pub enum PowerState {
	/// Sensor enabled any time the host is powered on.
	On,
	/// Sensor enabled When the host is powered on and has completed its BIOS POST phase.
	BiosPost,
	/// Sensor always enabled, regardless of host state.
	Always,
	/// Sensor enabled when host chassis power is on.
	ChassisOn,
}

impl PowerState {
	/// Test if a sensor whose power state is `self` should currently be enabled,
	/// based on our tracking of the host's current power state.
	pub fn active_now(&self) -> bool {
		let host = HOST_STATE.lock().unwrap();
		match self {
			Self::On => host.power_on,
			Self::BiosPost => host.power_on && host.post_complete,
			Self::Always => true,
			Self::ChassisOn => host.chassis_on,
		}
	}
}

impl TryFrom<&String> for PowerState {
	type Error = Box<dyn std::error::Error>;
	fn try_from(s: &String) -> ErrResult<Self> {
		match s.as_ref() {
			"Always" => Ok(Self::Always),
			"On" => Ok(Self::On),
			"BiosPost" => Ok(Self::BiosPost),
			"ChassisOn" => Ok(Self::ChassisOn),
			_ => Err(err_invalid_data("PowerState must be \"Always\", \"On\", \"BiosPost\", or \"ChassisOn\"")),
		}
	}
}

/// Represents various attributes of a host system's power state.
pub struct HostState {
	/// Whether or not chassis power is on.
	pub chassis_on: bool,
	/// Whether or not host system power is on.
	pub power_on: bool,
	/// Whether or not the host system has completed its POST phase.
	pub post_complete: bool,
}

/// The current power state of the host.
static HOST_STATE: SyncMutex<HostState> = SyncMutex::new(HostState {
	chassis_on: false,
	power_on: false,
	post_complete: false,
});

/// A collection of information pertaining to the dbus representation of an attribute of
/// host power state.
pub struct DBusPowerStateProperty {
	/// The bus name of the service from which we obtain the current state.
	pub busname: &'static str,
	/// The path of the object we retrieve the state from.
	pub path: &'static str,
	/// The interface of the object via which we retrieve the state.
	pub interface: &'static str,
	/// The specific property within the interface that we query to retrieve the state.
	pub property: &'static str,

	/// Which [`PowerState`] this property pertains to.  The datatype here is a slight
	/// kludge in that [`PowerState`] is mostly intended to represent a setting of a
	/// sensor rather than something about the host, but it's a close-enough fit that
	/// I'm reusing it at least for now...
	pub power_state: PowerState,

	/// How to map the string retrieved from dbus to the on/off state of the attribute.
	pub is_active: fn(s: &str) -> bool,
	/// How to update this property within a [`HostState`].
	pub update: fn(st: &mut HostState, new: bool),
}

impl DBusPowerStateProperty {
	/// Retrieve the current state of the property represented by `self` from dbus.
	pub async fn get(&self, bus: &nonblock::SyncConnection) -> ErrResult<bool> {
		let p = nonblock::Proxy::new(self.busname, self.path,
					     std::time::Duration::from_secs(30), bus);
		p.get::<String>(self.interface, self.property).await
			.map(|s| (self.is_active)(&s))
			.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
	}
}

mod dbus_properties {
	//! A small collection of static [`DBusPowerStateProperty`] instances.
	use super::{PowerState, DBusPowerStateProperty};

	/// Host chassis power state.
	pub const CHASSIS: DBusPowerStateProperty = DBusPowerStateProperty {
		busname: "xyz.openbmc_project.State.Chassis",
		path: "/xyz/openbmc_project/state/chassis0",
		interface: "xyz.openbmc_project.State.Chassis",
		property: "CurrentPowerState",

		power_state: PowerState::ChassisOn,
		is_active: |s| s.ends_with("On"),
		update: |s, n| { s.chassis_on = n; }
	};

	/// Host system power state.
	pub const POWER: DBusPowerStateProperty = DBusPowerStateProperty {
		busname: "xyz.openbmc_project.State.Host",
		path: "/xyz/openbmc_project/state/host0",
		interface: "xyz.openbmc_project.State.Host",
		property: "CurrentHostState",

		power_state: PowerState::On,
		is_active: |s| s.ends_with(".Running"),
		update: |s, n| { s.power_on = n; }
	};

	/// Host POST state.
	pub const POST: DBusPowerStateProperty = DBusPowerStateProperty {
		busname: "xyz.openbmc_project.State.OperatingSystem",
		path: "/xyz/openbmc_project/state/os",
		interface: "xyz.openbmc_project.State.OperatingSystem.Status",
		property: "OperatingSystemState",

		power_state: PowerState::BiosPost,
		is_active: |s| s != "Inactive" && s != "xyz.openbmc_project.State.OperatingSystem.Status.OSStatus.Inactive",
		update: |s, n| { s.post_complete = n; }
	};
}

/// Initialize [`HOST_STATE`] from dbus.
pub async fn init_host_state(bus: &nonblock::SyncConnection) {
	use dbus_properties::*;

	let chassis = CHASSIS.get(bus);
	let power = POWER.get(bus);
	let post = POST.get(bus);

	let retrieve = |res: ErrResult<bool>, name| {
		res.unwrap_or_else(|e| {
			eprintln!("Failed to retrieve {} status from dbus: {}", name, e);
			false // on error, assume things are off.
		})
	};

	let chassis = retrieve(chassis.await, "chassis");
	let power = retrieve(power.await, "host power");
	let post = retrieve(post.await, "POST");

	let mut state = HOST_STATE.lock().unwrap();
	let state = state.deref_mut();
	state.chassis_on = chassis;
	state.power_on = power;
	state.post_complete = post;
}

/// Register a callback `cb` to called on host power state transitions.
///
/// `cb` will be called with the attribute of power state that changed (again kind of a
/// hack datatype-wise, see [`DBusPowerStateProperty::power_state`]) and the new state of
/// that attribute (`true` for on, `false` for off).
pub async fn register_power_signal_handler<F, R>(bus: &nonblock::SyncConnection, cb: F) -> ErrResult<Vec<nonblock::MsgMatch>>
	where F: FnOnce(PowerState, bool) -> R + Send + Copy + Sync + 'static,
	      R: futures::Future<Output = ()> + Send
{
	use dbus_properties::*;
	use dbus::message::SignalArgs;
	use nonblock::stdintf::org_freedesktop_dbus::PropertiesPropertiesChanged as PPC;
	use futures::StreamExt;

	let mut signals = vec![];

	for prop in &[CHASSIS, POWER, POST] {
		let rule = MatchRule::new_signal(PPC::INTERFACE, PPC::NAME)
			.with_path(prop.path);
		let (signal, stream) = bus.add_match(rule).await?.stream();
		let stream = stream.for_each(move |(_, (intf, props)): (_, (String, dbus::arg::PropMap))| async move {
			if intf != prop.interface {
				return; // FIXME: eprintln!()?
			}

			let Some(newstate) = dbus::arg::prop_cast::<String>(&props, prop.property)
				.map(|s| (prop.is_active)(s)) else {
				return;
			};

			{
				let mut hoststate = HOST_STATE.lock().unwrap();
				let hoststate = hoststate.deref_mut();
				(prop.update)(hoststate, newstate);
			}

			tokio::spawn(async move { cb(prop.power_state, newstate).await });
		});
		signals.push(signal);
		tokio::spawn(async { stream.await });
	}

	Ok(signals)
}
