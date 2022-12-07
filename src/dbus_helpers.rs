use std::sync::Arc;
use dbus::{
	channel::Sender,
	nonblock::SyncConnection,
};

use crate::types::*;

struct PropChgCallback {
	msgfn: Arc<PropChgMsgFn>,
	dbuspath: Arc<dbus::Path<'static>>,
	conn: Arc<SyncConnection>,
}

// A dbus property that owns its own data and automatically sends
// property-change signals on updates
pub struct AutoProp<A> {
	data: A,
	callback: Option<PropChgCallback>,
}

impl<A: Copy + PartialEq + dbus::arg::RefArg> AutoProp<A> {
	pub fn new(data: A) -> Self {
		Self {
			data,
			callback: None,
		}
	}

	pub fn arm(&mut self, conn: &Arc<SyncConnection>, dbuspath: &Arc<dbus::Path<'static>>, msgfn: &Arc<PropChgMsgFn>) {
		if self.callback.is_some() {
			// oog, FIXME
			panic!("already-armed AutoProp re-armed");
		}
		self.callback = Some(PropChgCallback {
			msgfn: msgfn.clone(),
			dbuspath: dbuspath.clone(),
			conn: conn.clone(),
		})
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
		let cb = match &self.callback {
			Some(cb) => cb,
			_ => return,
		};

		if let Some(msg) = (cb.msgfn)(&cb.dbuspath, &dbus::arg::Variant(self.data)) {
			if cb.conn.send(msg).is_err() {
				eprintln!("Failed to send PropertiesChanged message for {}", cb.dbuspath);
			}
		} else {
			eprintln!("Failed to create PropertiesChanged message for {}", cb.dbuspath);
		}
	}
}
