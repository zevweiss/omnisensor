use std::sync::Arc;
use dbus::{
	channel::Sender,
	nonblock::SyncConnection,
};

use crate::types::*;

// A dbus property that owns its own data and automatically sends
// property-change signals on updates
pub struct SignalProp<A> {
	data: A,
	msgfn: Arc<PropChgMsgFn>,
	dbuspath: Arc<SensorPath>,
	conn: Arc<SyncConnection>,
}

impl<A: Copy + PartialEq + dbus::arg::RefArg> SignalProp<A> {
	pub fn new(data: A, msgfn: &Arc<PropChgMsgFn>, dbuspath: &Arc<SensorPath>, conn: &Arc<SyncConnection>) -> Self {
		Self {
			data,
			msgfn: msgfn.clone(),
			dbuspath: dbuspath.clone(),
			conn: conn.clone(),
		}
	}

	pub fn get(&self) -> A {
		self.data
	}

	pub fn set(&mut self, newdata: A) {
		let olddata = std::mem::replace(&mut self.data, newdata);
		if newdata != olddata {
			self.send_propchg();
		}
	}

	fn send_propchg(&self) {
		if let Some(msg) = (self.msgfn)(&self.dbuspath.0, &dbus::arg::Variant(self.data)) {
			if self.conn.send(msg).is_err() {
				eprintln!("Failed to send PropertiesChanged message for {}", self.dbuspath.0);
			}
		} else {
			eprintln!("Failed to create PropertiesChanged message for {}", self.dbuspath.0);
		}
	}
}

pub mod props {
	use super::*;
	use std::error::Error;
	use dbus::arg::PropMap;

	// Like dbus::arg::prop_cast, but more fine-grained so we can distinguish cases
	// we want to default (key not present) from ones we want to flag as an error
	// (key present but invalid):
	//   Ok(Some(T)) if present and valid
	//   Ok(None) if absent
	//   Err(_) if present but not a valid T
	pub fn prop_get_optional<'a, T: 'static>(map: &'a PropMap, key: &str) -> ErrResult<Option<&'a T>> {
		let Some(value) = map.get(key) else {
			return Ok(None);
		};
		let v = dbus::arg::cast::<T>(value);
		if v.is_some() {
			Ok(v)
		} else {
			Err(err_invalid_data(format!("invalid value for '{}' key", key)))
		}
	}

	// Like the above, but eliminates the inner Option, turning missing keys into errors.
	pub fn prop_get_mandatory<'a, T: 'static>(map: &'a PropMap, key: &str) -> ErrResult<&'a T> {
		match prop_get_optional(map, key) {
			Ok(Some(v)) => Ok(v),
			Ok(None) => Err(err_not_found(format!("required key '{}' not found", key))),
			Err(e) => Err(e),
		}
	}

	// Like the above, but returns a provided default value if the key is absent.
	pub fn prop_get_default<'a, T: 'static>(map: &'a PropMap, key: &str, default: &'a T) -> ErrResult<&'a T> {
		match prop_get_optional(map, key) {
			Ok(Some(v)) => Ok(v),
			Ok(None) => Ok(default),
			Err(e) => Err(e),
		}
	}

	// Like prop_get_optional, but convert to something more specific via TryFrom.
	pub fn prop_get_optional_from<'a, I, T>(map: &'a PropMap, key: &str) -> ErrResult<Option<T>>
	where T: TryFrom<&'a I, Error = Box<dyn Error>>, I: 'a + ?Sized + 'static
	{
		match prop_get_optional::<&I>(map, key) {
			Ok(Some(v)) => Ok(Some(T::try_from(v)?)),
			Ok(None) => Ok(None),
			Err(e) => Err(e),
		}
	}

	// Like prop_get_mandatory, but convert to something more specific via TryFrom.
	pub fn prop_get_mandatory_from<'a, I, T>(map: &'a PropMap, key: &str) -> ErrResult<T>
	where T: TryFrom<&'a I, Error = Box<dyn Error>>, I: 'a + ?Sized + 'static
	{
		match prop_get_mandatory::<&I>(map, key) {
			Ok(v) => Ok(T::try_from(v)?),
			Err(e) => Err(e),
		}
	}

	// Like prop_get_default, but convert to something more specific via TryFrom.
	pub fn prop_get_default_from<'a, I, T>(map: &'a PropMap, key: &str, default: T) -> ErrResult<T>
	where T: TryFrom<&'a I, Error = Box<dyn Error>>, I: 'a + ?Sized + 'static
	{
		match prop_get_optional::<&I>(map, key) {
			Ok(Some(v)) => Ok(T::try_from(v)?),
			Ok(None) => Ok(default),
			Err(e) => Err(e),
		}
	}
}
