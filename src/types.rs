//! Relatively generic convenience types.

use std::{
	collections::HashSet,
	error::Error,
	io::ErrorKind,
	sync::Arc,
};
use tokio::sync::Mutex;
use crate::sensor::Sensor;

/// A generic Result type for things that can fail.
pub type ErrResult<T> = Result<T, Box<dyn Error>>;

/// Internal helper for concisely creating an ErrResult.
fn mk_err<E>(kind: ErrorKind, err: E) -> Box<dyn Error>
	where E: Into<Box<dyn Error + Send + Sync>>
{
	Box::new(std::io::Error::new(kind, err))
}

/// Construct an `Error` of kind `ErrorKind::NotFound` with the given payload.
pub fn err_not_found<E>(msg: E) -> Box<dyn Error>
	where E: Into<Box<dyn Error + Send + Sync>>
{
	mk_err(ErrorKind::NotFound, msg)
}

/// Construct an `Error` of kind `ErrorKind::InvalidData` with the given payload.
pub fn err_invalid_data<E>(msg: E) -> Box<dyn Error>
	where E: Into<Box<dyn Error + Send + Sync>>
{
	mk_err(ErrorKind::InvalidData, msg)
}

/// Construct an `Error` of kind `ErrorKind::Unsupported` with the given payload.
pub fn err_unsupported<E>(msg: E) -> Box<dyn Error>
	where E: Into<Box<dyn Error + Send + Sync>>
{
	mk_err(ErrorKind::Unsupported, msg)
}

/// Construct an `Error` of kind `ErrorKind::Other` with the given payload.
pub fn err_other<E>(msg: E) -> Box<dyn Error>
	where E: Into<Box<dyn Error + Send + Sync>>
{
	mk_err(ErrorKind::Other, msg)
}

/// A data structure to represent an optional allowlist.  This _could_ just be
/// an [`Option`]<[`HashSet`]\<T>> with [`None`] meaning "allow everything", but
/// this is a bit more explicit (and "none means all" is sort of
/// counterintuitive, after all...), and allows more natural, readable querying
/// via [`FilterSet::contains()`].
#[derive(Debug)]
pub enum FilterSet<T> {
	/// A [`FilterSet`] that allows everything.
	All,

	/// A [`FilterSet`] that allows on a specific set of things.
	Only(HashSet<T>),
}

impl<T: Eq + std::hash::Hash> FilterSet<T> {
	/// If `self` is [`Only`](FilterSet::Only), returns `true` if `x` is
	/// contained in the set, and `false` otherwise.  If `self` is
	/// [`All`](FilterSet::All), returns `true` for any `x`.
	pub fn contains(&self, x: &T) -> bool {
		match self {
			Self::All => true,
			Self::Only(set) => set.contains(x),
		}
	}
}

impl<T> From<Option<HashSet<T>>> for FilterSet<T> {
	/// If the [`Option`] is [`Some`] but the set is empty, you get what you
	/// asked for (a filter that rejects everything).
	fn from(set: Option<HashSet<T>>) -> Self {
		match set {
			Some(set) => Self::Only(set),
			None => Self::All,
		}
	}
}

/// Alias for functions for generating dbus `PropertiesChanged` messages.
pub type PropChgMsgFn = dyn Fn(&dbus::Path<'_>, &dyn dbus::arg::RefArg) -> Option<dbus::Message> + Send + Sync;

/// A wrapper type for a [`dbus_crossroads`] dbus interface.  The intent is that
/// `T` is a collection of `PropChgMsgFn`s, one per property of the interface.
pub struct SensorIntf<T> {
	pub token: dbus_crossroads::IfaceToken<Arc<Mutex<Sensor>>>,
	pub msgfns: T,
}

/// A newtype for a dbus path to reduce the likelihood of mixing up inventory
/// paths and sensor paths.
#[derive(PartialEq, Eq, Hash)]
pub struct InventoryPath(pub dbus::Path<'static>);

/// A newtype for a dbus path to reduce the likelihood of mixing up inventory
/// paths and sensor paths.
#[derive(Clone)]
pub struct SensorPath(pub dbus::Path<'static>);
