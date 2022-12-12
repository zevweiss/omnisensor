// Relatively generic convenience types

use std::{
	collections::HashSet,
	error::Error,
	io::ErrorKind,
	sync::Arc,
};
use tokio::sync::Mutex;
use crate::sensor::Sensor;

pub type ErrResult<T> = Result<T, Box<dyn Error>>;

// Not strictly types, but convenient helpers to use for ErrResults
fn mk_err<E>(kind: ErrorKind, err: E) -> Box<dyn Error>
	where E: Into<Box<dyn Error + Send + Sync>>
{
	Box::new(std::io::Error::new(kind, err))
}

pub fn err_not_found<E>(msg: E) -> Box<dyn Error>
	where E: Into<Box<dyn Error + Send + Sync>>
{
	mk_err(ErrorKind::NotFound, msg)
}

pub fn err_invalid_data<E>(msg: E) -> Box<dyn Error>
	where E: Into<Box<dyn Error + Send + Sync>>
{
	mk_err(ErrorKind::InvalidData, msg)
}

pub fn err_other<E>(msg: E) -> Box<dyn Error>
	where E: Into<Box<dyn Error + Send + Sync>>
{
	mk_err(ErrorKind::Other, msg)
}

// Could just use Option<T> for this with None meaning all, but this is a bit
// more explicit ("None == All" is sorta counterintuitive, after all...), and
// allows more natural, readable querying via FilterSet::contains().
#[derive(Debug)]
pub enum FilterSet<T> {
	All,
	Only(HashSet<T>),
}

impl<T: Eq + std::hash::Hash> FilterSet<T> {
	pub fn contains(&self, x: &T) -> bool {
		match self {
			Self::All => true,
			Self::Only(set) => set.contains(x),
		}
	}
}

// If the Option is Some but the set is empty, you get what you asked for (a
// filter that rejects everything).
impl<T> From<Option<HashSet<T>>> for FilterSet<T> {
	fn from(set: Option<HashSet<T>>) -> Self {
		match set {
			Some(set) => Self::Only(set),
			None => Self::All,
		}
	}
}

pub type PropChgMsgFn = dyn Fn(&dbus::Path<'_>, &dyn dbus::arg::RefArg) -> Option<dbus::Message> + Send + Sync;

pub struct SensorIntf<T> {
	pub token: dbus_crossroads::IfaceToken<Arc<Mutex<Sensor>>>,
	pub msgfns: T,
}

// Use these to ensure the two don't get mixed up.
#[derive(PartialEq, Eq, Hash)]
pub struct InventoryPath(pub dbus::Path<'static>);
#[derive(Clone)]
pub struct SensorPath(pub dbus::Path<'static>);
