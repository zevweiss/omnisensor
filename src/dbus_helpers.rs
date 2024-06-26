//! Assorted utilities for easing dbus usage.

use std::sync::Arc;
use log::error;
use dbus::{
	channel::Sender,
	nonblock::SyncConnection,
};

use crate::types::*;

/// A dbus property that owns its own data and automatically sends
/// property-change signals on updates.
pub struct SignalProp<A> {
	/// The current value of the property.
	data: A,
	/// A function to create a `PropetiesChanged` signal message when `data` changes.
	msgfn: Arc<PropChgMsgFn>,
	/// The dbus path of the object this property belongs to.
	dbuspath: Arc<SensorPath>,
	/// The dbus connection via which `PropetiesChanged` signals are sent.
	conn: Arc<SyncConnection>,
}

impl<A: PartialEq + dbus::arg::RefArg> SignalProp<A> {
	/// Construct a [`SignalProp`] with the given members.
	pub fn new(data: A, msgfn: &Arc<PropChgMsgFn>, dbuspath: &Arc<SensorPath>,
	           conn: &Arc<SyncConnection>) -> Self {
		Self {
			data,
			msgfn: msgfn.clone(),
			dbuspath: dbuspath.clone(),
			conn: conn.clone(),
		}
	}

	/// Retrieve `self`'s current value.
	pub fn get(&self) -> A
	where A: Copy
	{
		self.data
	}

	/// Retrieve a clone of `self`'s current value.
	pub fn get_clone(&self) -> A
	where A: Clone
	{
		self.data.clone()
	}

	/// Update `self`'s value to `newdata`, emitting a `PropertiesChanged`
	/// signal to dbus if `newdata` is unequal to `self.data`'s previous
	/// value.
	pub fn set(&mut self, newdata: A) {
		let changed = newdata != self.data;
		self.data = newdata;
		if changed {
			self.send_propchg();
		}
	}

	/// Emit a `PropertiesChanged` signal to dbus with `self`'s current value.
	fn send_propchg(&self) {
		if let Some(msg) = (self.msgfn)(&self.dbuspath.0, &self.data) {
			if self.conn.send(msg).is_err() {
				error!("Failed to send PropertiesChanged message for {:?}",
				       self.dbuspath);
			}
		} else {
			error!("Failed to create PropertiesChanged message for {:?}",
			       self.dbuspath);
		}
	}
}

pub mod props {
	//! A collection of [`prop_cast()`](dbus::arg::prop_cast)-like helper functions
	//! for retrieving data from dbus [`PropMap`](dbus::arg::PropMap)s.
	use super::*;
	use std::error::Error;
	use dbus::arg::PropMap;

	/// Similar to [`dbus::arg::prop_cast()`], but more fine-grained.
	///
	/// So that the caller can distinguish cases we want to default (key not present)
	/// from ones we want to flag as an error (key present but invalid), return values
	/// are as follows:
	///
	///   * `Ok(Some(_))` if the key is present and the corresponding value is valid.
	///   * `Ok(None)` if the key is absent.
	///   * `Err(_)` if the key is present but the value is not a valid `T`.
	pub fn prop_get_optional<'a, T: 'static>(map: &'a PropMap, key: &str)
	                                         -> ErrResult<Option<&'a T>>
	{
		let Some(value) = map.get(key) else {
			return Ok(None);
		};
		let v = dbus::arg::cast::<T>(&value.0);
		if v.is_some() {
			Ok(v)
		} else {
			Err(err_invalid_data(format!("invalid value for '{}' key", key)))
		}
	}

	/// Like [`prop_get_optional()`], but eliminates the inner [`Option`] by turning
	/// missing keys into errors.
	pub fn prop_get_mandatory<'a, T: 'static>(map: &'a PropMap, key: &str) -> ErrResult<&'a T> {
		match prop_get_optional(map, key) {
			Ok(Some(v)) => Ok(v),
			Ok(None) => Err(err_not_found(format!("required key '{}' not found", key))),
			Err(e) => Err(e),
		}
	}

	/// Like [`prop_get_optional()`], but eliminates the inner [`Option`] by returning
	/// a provided default value if the key is absent.
	pub fn prop_get_default<'a, T: 'static>(map: &'a PropMap, key: &str, default: &'a T)
	                                        -> ErrResult<&'a T>
	{
		match prop_get_optional(map, key) {
			Ok(Some(v)) => Ok(v),
			Ok(None) => Ok(default),
			Err(e) => Err(e),
		}
	}

	/// Like [`prop_get_optional()`], but converts to a more specific type `T` via an
	/// intermediate type `I` using [`TryFrom`].
	///
	/// For example, an enum represented on dbus as a string can be easily converted
	/// to its internal representation as long as the internal enum implements
	/// `TryFrom<&String>`.  (`I` is most often [`String`], but does not have to be.)
	pub fn prop_get_optional_from<'a, I, T>(map: &'a PropMap, key: &str) -> ErrResult<Option<T>>
	where T: TryFrom<&'a I, Error = Box<dyn Error>>, I: 'static
	{
		match prop_get_optional::<I>(map, key) {
			Ok(Some(v)) => Ok(Some(T::try_from(v)?)),
			Ok(None) => Ok(None),
			Err(e) => Err(e),
		}
	}

	/// Like [`prop_get_optional_from()`], but eliminates the inner [`Option`] by
	/// turning missing keys into errors.
	pub fn prop_get_mandatory_from<'a, I, T>(map: &'a PropMap, key: &str) -> ErrResult<T>
	where T: TryFrom<&'a I, Error = Box<dyn Error>>, I: 'static
	{
		match prop_get_mandatory::<I>(map, key) {
			Ok(v) => Ok(T::try_from(v)?),
			Err(e) => Err(e),
		}
	}

	/// Like [`prop_get_optional_from()`], but eliminates the inner [`Option`] by
	/// returning a provided default if the key is absent.
	pub fn prop_get_default_from<'a, I, T>(map: &'a PropMap, key: &str, default: T)
	                                       -> ErrResult<T>
	where T: TryFrom<&'a I, Error = Box<dyn Error>>, I: 'static
	{
		match prop_get_optional::<I>(map, key) {
			Ok(Some(v)) => Ok(T::try_from(v)?),
			Ok(None) => Ok(default),
			Err(e) => Err(e),
		}
	}

	/// A special case of [`prop_get_default()`] for inconsistently-typed numeric
	/// values.
	///
	/// An unfortunate aspect of entity-manager is that some numerically-valued
	/// properties (`PollRate`, for example) aren't always the same type, sometimes
	/// presented as a u64 (the dbus `t` type) and sometimes as an f64 (the dbus `d`
	/// type).  This papers over the inconsistency by first trying to find a float but
	/// falling back to checking for a uint and converting it into a float if that
	/// fails.
	pub fn prop_get_default_num(map: &PropMap, key: &str, default: f64) -> ErrResult<f64> {
		match prop_get_default::<f64>(map, key, &default) {
			r @ Ok(_) => r.copied(),
			e @ Err(_) => {
				let idef = default as u64;
				match prop_get_default::<u64>(map, key, &idef) {
					Ok(u) => {
						let f = *u as f64;
						// we're probably pretty unlikely to see
						// integer values that can't be exactly
						// represented as an f64, but let's make
						// absolutely sure and fail if it can't.
						if f as u64 == *u {
							Ok(f)
						} else {
							let m = format!("uint value {} for '{}' \
							                 not representable as f64",
							                *u, key);
							Err(err_invalid_data(m))
						}
					}

					// return the original error (from the float
					// retrieval attempt) if both float and int fail.
					Err(_) => e.copied(),
				}
			},
		}
	}
}
