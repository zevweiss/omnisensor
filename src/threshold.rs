use std::{
	collections::HashMap,
	sync::Arc,
};
use tokio::sync::Mutex;
use dbus::arg::prop_cast;
use dbus_crossroads::{
	Crossroads,
	IfaceBuilder,
	MethodErr,
};
use strum::IntoEnumIterator;
use strum_macros::EnumIter;

use crate::{
	sensor::{
		DBusSensor,
		DBusSensorState,
	},
	types::*,
};

#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, EnumIter)]
pub enum ThresholdSeverity {
	Warning,
	Critical,
	PerformanceLoss,
	SoftShutdown,
	HardShutdown,
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

	fn to_str(&self) -> &'static str {
		match self {
			Self::Warning => "Warning",
			Self::Critical => "Critical",
			Self::PerformanceLoss => "PerformanceLoss",
			Self::SoftShutdown => "SoftShutdown",
			Self::HardShutdown => "HardShutdown",
		}
	}
}

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
			.map(|s| ThresholdBoundType::from_str(s))
			.flatten()?;
		let severity = prop_cast::<f64>(props, "Severity")
			.map(|n| ThresholdSeverity::from_f64(*n))
			.flatten()?;
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
		let props = match intfs.get(&intf) {
			Some(pm) => pm,
			_ => break,
		};
		if let Some(thr) = ThresholdConfig::from_dbus(&props) {
			thresholds.push(thr);
		}
	}
	thresholds
}

pub struct ThresholdBound {
	value: f64,
	hysteresis: f64,
	alarm: bool,
}

impl ThresholdBound {
	fn update(&mut self, kind: ThresholdBoundType, sample: f64) {
		if !sample.is_finite() || !self.value.is_finite() {
			return;
		}

		// FIXME: factor in hysteresis
		self.alarm = match kind {
			ThresholdBoundType::Upper => sample >= self.value,
			ThresholdBoundType::Lower => sample <= self.value,
		};
	}
}

impl Default for ThresholdBound {
	fn default() -> Self {
		Self {
			value: f64::NAN,
			hysteresis: f64::NAN,
			alarm: false,
		}
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

pub type Thresholds = HashMap<ThresholdSeverity, Threshold>;

pub fn get_thresholds_from_configs(cfgs: &[ThresholdConfig]) -> Thresholds {
	let mut thresholds = Thresholds::new();
	for cfg in cfgs {
		let threshold = thresholds.entry(cfg.severity).or_insert(Threshold {
			low: Default::default(),
			high: Default::default(),
		});

		let bound = match cfg.kind {
			ThresholdBoundType::Upper => &mut threshold.high,
			ThresholdBoundType::Lower => &mut threshold.low,
		};

		let set_f64 = |loc: &mut f64, val: f64, name| {
			if !val.is_finite() {
				return;
			}
			if loc.is_finite() {
				eprintln!("Warning: multiple {:?} {:?} bound {} values specified, overriding {} with {}",
					  cfg.severity, cfg.kind, name, *loc, val);
			}
			*loc = val;
		};

		set_f64(&mut bound.value, cfg.value, "threshold");
		set_f64(&mut bound.hysteresis, cfg.hysteresis, "hysteresis");
	}
	thresholds
}

fn get_prop_value<F, R>(mut ctx: dbus_crossroads::PropContext, dbs: &Arc<Mutex<DBusSensor>>, sev: ThresholdSeverity, getter: F)
			-> impl futures::Future<Output = std::marker::PhantomData<R>>
	where F: Fn(&Threshold) -> R, R: dbus::arg::Arg + dbus::arg::RefArg + dbus::arg::Append + 'static
{
	let dbs = dbs.clone();
	async move {
		let dbs = dbs.lock().await;
		let mut sendvalue = |sevmap: &Thresholds| {
			match sevmap.get(&sev) {
				Some(t) => ctx.reply(Ok(getter(t))),
				None => ctx.reply(Err(MethodErr::failed("no threshold for interface"))),
			}
		};

		match &dbs.state {
			DBusSensorState::Active(s) => sendvalue(&s.lock().await.thresholds),
			DBusSensorState::Phantom(p) => sendvalue(&p.thresholds),
		}
	}
}

fn build_sensor_threshold_intf(cr: &mut Crossroads, sev: ThresholdSeverity) -> SensorIntfToken {
	let sevstr = sev.to_str();
	let intfname = format!("xyz.openbmc_project.Sensor.Threshold.{}", sevstr);

	cr.register(intfname, |b: &mut IfaceBuilder<Arc<Mutex<DBusSensor>>>| {
		b.property(format!("{}High", sevstr))
			.get_async(move |ctx, dbs| get_prop_value(ctx, dbs, sev, |t| t.high.value));
		b.property(format!("{}Low", sevstr))
			.get_async(move |ctx, dbs| get_prop_value(ctx, dbs, sev, |t| t.low.value));
		b.property(format!("{}AlarmHigh", sevstr))
			.get_async(move |ctx, dbs| get_prop_value(ctx, dbs, sev, |t| t.high.alarm));
		b.property(format!("{}AlarmLow", sevstr))
			.get_async(move |ctx, dbs| get_prop_value(ctx, dbs, sev, |t| t.low.alarm));
	})
}

pub fn build_sensor_threshold_intfs(cr: &mut Crossroads) -> HashMap<ThresholdSeverity, SensorIntfToken> {
	ThresholdSeverity::iter()
		.map(|sev| (sev, build_sensor_threshold_intf(cr, sev)))
		.collect()
}
