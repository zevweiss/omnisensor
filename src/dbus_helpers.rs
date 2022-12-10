use std::sync::Arc;
use dbus::{
	channel::Sender,
	nonblock::SyncConnection,
};

use crate::types::*;

// A dbus property that owns its own data and automatically sends
// property-change signals on updates
pub struct AutoProp<A> {
	data: A,
	msgfn: Arc<PropChgMsgFn>,
	dbuspath: Arc<SensorPath>,
	conn: Arc<SyncConnection>,
}

impl<A: Copy + PartialEq + dbus::arg::RefArg> AutoProp<A> {
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
