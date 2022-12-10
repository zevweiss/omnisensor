use std::{
	collections::HashMap,
	sync::Arc,
};
use tokio::sync::Mutex;
use dbus::{
	arg::prop_cast,
	nonblock::SyncConnection,
};
use dbus_crossroads::{
	Crossroads,
	IfaceBuilder,
	MethodErr,
};
use strum::{EnumCount as _EnumCount, IntoEnumIterator};
use strum_macros::{EnumCount, EnumIter};

use crate::{
	sensor::Sensor,
	types::*,
	dbus_helpers::AutoProp,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, EnumIter, EnumCount)]
pub enum ThresholdSeverity {
	Warning = 0,
	Critical = 1,
	PerformanceLoss = 2,
	SoftShutdown = 3,
	HardShutdown = 4,
}

impl ThresholdSeverity {
	// dbus config presents this as a float instead of an int for
	// some reason...
	fn from_f64(n: f64) -> Option<Self> {
		match n as isize {
			0 => Some(Self::Warning),
			1 => Some(Self::Critical),
			2 => Some(Self::PerformanceLoss),
			3 => Some(Self::SoftShutdown),
			4 => Some(Self::HardShutdown),
			_ => None,
		}
	}

	fn to_str(self) -> &'static str {
		match self {
			Self::Warning => "Warning",
			Self::Critical => "Critical",
			Self::PerformanceLoss => "PerformanceLoss",
			Self::SoftShutdown => "SoftShutdown",
			Self::HardShutdown => "HardShutdown",
		}
	}
}

type ThresholdSeverityArray<T> = [T; ThresholdSeverity::COUNT];

#[derive(Debug, Copy, Clone)]
pub enum ThresholdBoundType {
	Upper,
	Lower,
}

impl ThresholdBoundType {
	fn from_str(s: &str) -> Option<Self> {
		match s {
			"less than" => Some(Self::Lower),
			"greater than" => Some(Self::Upper),
			_ => None,
		}
	}
}

#[derive(Debug, Copy, Clone)]
pub struct ThresholdConfig {
	// E-M config also includes a "Name" field, but it doesn't
	// appear to be used for anything that I can see...
	kind: ThresholdBoundType,
	severity: ThresholdSeverity,
	hysteresis: f64,
	value: f64,
}

impl ThresholdConfig {
	fn from_dbus(props: &dbus::arg::PropMap) -> Option<Self> {
		// TODO: issue errors on missing/invalid config keys
		let kind = prop_cast::<String>(props, "Direction")
			.and_then(|s| ThresholdBoundType::from_str(s))?;
		let severity = prop_cast::<f64>(props, "Severity")
			.and_then(|n| ThresholdSeverity::from_f64(*n))?;
		let value = *prop_cast::<f64>(props, "Value")?;
		let hysteresis = *prop_cast::<f64>(props, "Hysteresis")
			.unwrap_or(&f64::NAN);

		if !value.is_finite() {
			return None;
		}

		Some(Self {
			kind,
			severity,
			value,
			hysteresis,
		})
	}
}

pub fn get_configs_from_dbus(baseintf: &str, intfs: &HashMap<String, dbus::arg::PropMap>) -> Vec<ThresholdConfig> {
	let mut thresholds = vec![];
	for tidx in 0.. {
		let intf = format!("{}.Thresholds{}", baseintf, tidx);
		let Some(props) = intfs.get(&intf) else {
			break;
		};
		if let Some(thr) = ThresholdConfig::from_dbus(props) {
			thresholds.push(thr);
		}
	}
	thresholds
}

pub struct ThresholdBound {
	value: AutoProp<f64>,
	hysteresis: f64,
	alarm: AutoProp<bool>,
}

impl ThresholdBound {
	fn new(msgfns: &ThresholdBoundIntfMsgFns, dbuspath: &Arc<SensorPath>, conn: &Arc<SyncConnection>) -> Self {
		Self {
			value: AutoProp::new(f64::NAN, &msgfns.value, dbuspath, conn),
			hysteresis: f64::NAN,
			alarm: AutoProp::new(false, &msgfns.alarm, dbuspath, conn),
		}
	}

	fn update(&mut self, kind: ThresholdBoundType, sample: f64) {
		if !sample.is_finite() || !self.value.get().is_finite() {
			return;
		}

		let (test, adjust): (fn(f64, f64) -> bool, _) = match kind {
			ThresholdBoundType::Upper => (|s, t| s >= t, -self.hysteresis),
			ThresholdBoundType::Lower => (|s, t| s <= t, self.hysteresis),
		};

		let mut threshold = self.value.get();
		if self.alarm.get() {
			threshold += adjust;
		}

		self.alarm.set(test(sample, threshold));
	}
}

pub struct Threshold {
	low: ThresholdBound,
	high: ThresholdBound,
}

impl Threshold {
	pub fn update(&mut self, sample: f64) {
		self.low.update(ThresholdBoundType::Lower, sample);
		self.high.update(ThresholdBoundType::Upper, sample);
	}
}

pub type ThresholdArr = ThresholdSeverityArray<Option<Threshold>>;

pub fn get_thresholds_from_configs(cfgs: &[ThresholdConfig], threshold_intfs: &ThresholdIntfDataArr,
				   dbuspath: &Arc<SensorPath>, conn: &Arc<SyncConnection>) -> ThresholdArr {
	let mut thresholds = ThresholdArr::default();
	for cfg in cfgs {
		let intf = &threshold_intfs[cfg.severity as usize];
		let threshold = thresholds[cfg.severity as usize].get_or_insert_with(|| Threshold {
			low: ThresholdBound::new(&intf.msgfns.low, dbuspath, conn),
			high: ThresholdBound::new(&intf.msgfns.high, dbuspath, conn),
		});

		let bound = match cfg.kind {
			ThresholdBoundType::Upper => &mut threshold.high,
			ThresholdBoundType::Lower => &mut threshold.low,
		};

		let check = |old: f64, new: f64, name| {
			if !new.is_finite() {
				return false;
			}
			if old.is_finite() {
				eprintln!("Warning: multiple {:?} {:?} bound {} values specified, overriding {} with {}",
					  cfg.severity, cfg.kind, name, old, new);
			}
			true
		};

		if check(bound.value.get(), cfg.value, "threshold") {
			bound.value.set(cfg.value);
		}

		if check(bound.hysteresis, cfg.hysteresis, "hysteresis") {
			bound.hysteresis = cfg.hysteresis;
		}
	}
	thresholds
}

fn get_prop_value<F, R>(mut ctx: dbus_crossroads::PropContext, sensor: &Arc<Mutex<Sensor>>, sev: ThresholdSeverity, getter: F)
			-> impl futures::Future<Output = std::marker::PhantomData<R>>
	where F: Fn(&Threshold) -> R, R: dbus::arg::Arg + dbus::arg::RefArg + dbus::arg::Append + 'static
{
	let sensor = sensor.clone();
	async move {
		let sensor = sensor.lock().await;
		match &sensor.thresholds[sev as usize] {
			Some(t) => ctx.reply(Ok(getter(t))),
			None => ctx.reply(Err(MethodErr::failed("no threshold for interface"))),
		}
	}
}

struct ThresholdBoundIntfMsgFns {
	value: Arc<PropChgMsgFn>,
	alarm: Arc<PropChgMsgFn>,
}

pub struct ThresholdIntfMsgFns {
	high: ThresholdBoundIntfMsgFns,
	low: ThresholdBoundIntfMsgFns,
}

pub struct ThresholdIntfData {
	pub token: SensorIntfToken,
	pub msgfns: ThresholdIntfMsgFns,
}

pub type ThresholdIntfDataArr = ThresholdSeverityArray<ThresholdIntfData>;

fn build_threshold_bound_intf<F>(b: &mut IfaceBuilder<Arc<Mutex<Sensor>>>, sev: ThresholdSeverity, tag: &str, getter: F) -> ThresholdBoundIntfMsgFns
	where F: Fn(&Threshold) -> &ThresholdBound + Copy + Send + Sync + 'static
{
	let value = b.property(format!("{}{}", sev.to_str(), tag))
		.get_async(move |ctx, s| get_prop_value(ctx, s, sev, move |t| getter(t).value.get()))
		.emits_changed_true()
		.changed_msg_fn()
		.into();

	let alarm = b.property(format!("{}Alarm{}", sev.to_str(), tag))
		.get_async(move |ctx, s| get_prop_value(ctx, s, sev, move |t| getter(t).alarm.get()))
		.emits_changed_true()
		.changed_msg_fn()
		.into();

	ThresholdBoundIntfMsgFns {
		value,
		alarm,
	}
}

fn build_sensor_threshold_intf(cr: &mut Crossroads, sev: ThresholdSeverity) -> ThresholdIntfData {
	let mut propchg_msgfns = None;
	let sevstr = sev.to_str();
	let intfname = format!("xyz.openbmc_project.Sensor.Threshold.{}", sevstr);

	let token = cr.register(intfname, |b: &mut IfaceBuilder<Arc<Mutex<Sensor>>>| {
		propchg_msgfns = Some(ThresholdIntfMsgFns {
			high: build_threshold_bound_intf(b, sev, "High", |t| &t.high),
			low: build_threshold_bound_intf(b, sev, "low", |t| &t.low),
		});
	});

	ThresholdIntfData {
		token,
		msgfns: propchg_msgfns.expect("propchg_msgfns not set?"),
	}
}

pub fn build_sensor_threshold_intfs(cr: &mut Crossroads) -> ThresholdIntfDataArr {
	let res = ThresholdSeverity::iter()
		.map(|sev| build_sensor_threshold_intf(cr, sev))
		.collect::<Vec<_>>()
		.try_into();

	// .unwrap() unfortunately requires Debug, so do it manually...
	match res {
		Ok(a) => a,
		_ => panic!("ThresholdSeverity::iter() produced the wrong number of elements?"),
	}
}
