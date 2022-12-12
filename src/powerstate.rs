use std::ops::DerefMut;
use dbus::{
	message::MatchRule,
	nonblock,
	nonblock::stdintf::org_freedesktop_dbus::Properties,
};
use tokio::sync::Mutex;

use crate::types::*;

#[derive(Debug, Copy, Clone)]
pub enum PowerState {
	On,
	BiosPost,
	Always,
	ChassisOn,
}

impl PowerState {
	pub async fn active_now(&self) -> bool {
		let host = HOST_STATE.lock().await;
		match self {
			Self::On => host.power_on,
			Self::BiosPost => host.power_on && host.post_complete,
			Self::Always => true,
			Self::ChassisOn => host.chassis_on,
		}
	}
}

impl TryFrom<&str> for PowerState {
	type Error = Box<dyn std::error::Error>;
	fn try_from(s: &str) -> ErrResult<Self> {
		match s {
			"Always" => Ok(Self::Always),
			"On" => Ok(Self::On),
			"BiosPost" => Ok(Self::BiosPost),
			"ChassisOn" => Ok(Self::ChassisOn),
			_ => Err(err_invalid_data("PowerState must be \"Always\", \"On\", \"BiosPost\", or \"ChassisOn\"")),
		}
	}
}

pub struct HostState {
	pub chassis_on: bool,
	pub power_on: bool,
	pub post_complete: bool,
}

// FIXME: std::sync::Mutex::new is const in 1.63...tokio's has const_new() now though.
static HOST_STATE: Mutex<HostState> = Mutex::const_new(HostState {
	chassis_on: false,
	power_on: false,
	post_complete: false,
});

pub struct DBusPowerStateProperty {
	pub busname: &'static str,
	pub path: &'static str,
	pub interface: &'static str,
	pub property: &'static str,

	pub power_state: PowerState,
	pub is_active: fn(s: &str) -> bool,
	pub update: fn(st: &mut HostState, new: bool),
}

impl DBusPowerStateProperty {
	pub async fn get(&self, bus: &nonblock::SyncConnection) -> ErrResult<bool> {
		let p = nonblock::Proxy::new(self.busname, self.path,
					     std::time::Duration::from_secs(30), bus);
		p.get::<String>(self.interface, self.property).await
			.map(|s| (self.is_active)(&s))
			.map_err(|e| Box::new(e) as Box<dyn std::error::Error>)
	}
}

mod dbus_properties {
	use super::{PowerState, DBusPowerStateProperty};

	pub const CHASSIS: DBusPowerStateProperty = DBusPowerStateProperty {
		busname: "xyz.openbmc_project.State.Chassis",
		path: "/xyz/openbmc_project/state/chassis0",
		interface: "xyz.openbmc_project.State.Chassis",
		property: "CurrentPowerState",

		power_state: PowerState::ChassisOn,
		is_active: |s| s.ends_with("On"),
		update: |s, n| { s.chassis_on = n; }
	};

	pub const POWER: DBusPowerStateProperty = DBusPowerStateProperty {
		busname: "xyz.openbmc_project.State.Host",
		path: "/xyz/openbmc_project/state/host0",
		interface: "xyz.openbmc_project.State.Host",
		property: "CurrentHostState",

		power_state: PowerState::On,
		is_active: |s| s.ends_with(".Running"),
		update: |s, n| { s.power_on = n; }
	};

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

pub async fn init_host_state(bus: &nonblock::SyncConnection) {
	use dbus_properties::*;

	let chassis = CHASSIS.get(bus);
	let power = POWER.get(bus);
	let post = POST.get(bus);

	let retrieve = |res: ErrResult<bool>, name| {
		res.unwrap_or_else(|e| {
			eprintln!("Failed to retrieve {} status from dbus: {}", name, e);
			false
		})
	};

	let chassis = retrieve(chassis.await, "chassis");
	let power = retrieve(power.await, "host power");
	let post = retrieve(post.await, "POST");

	let mut state = HOST_STATE.lock().await;
	let state = state.deref_mut();
	state.chassis_on = chassis;
	state.power_on = power;
	state.post_complete = post;
}

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
				let mut hoststate = HOST_STATE.lock().await;
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
