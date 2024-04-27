//! Utility code for managing sensor thresholds.

use std::{
	collections::HashMap,
	sync::Arc,
};
use tokio::sync::Mutex;
use dbus::nonblock::SyncConnection;
use dbus_crossroads::{
	Crossroads,
	IfaceBuilder,
	MethodErr,
};
use strum::{EnumCount as _EnumCount, IntoEnumIterator};
use strum_macros::{EnumCount, EnumIter};
use log::{warn, error};

use crate::{
	sensor::Sensor,
	types::*,
	dbus_helpers::{
		props::*,
		SignalProp,
	},
};

/// The severity level of a threshold.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash, EnumIter, EnumCount)]
pub enum ThresholdSeverity {
	Warning = 0,
	Critical = 1,
	PerformanceLoss = 2,
	SoftShutdown = 3,
	HardShutdown = 4,
}

impl ThresholdSeverity {
	/// Return the string form of a severity level.
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

impl TryFrom<&f64> for ThresholdSeverity {
	type Error = Box<dyn std::error::Error>;
	/// dbus config for some reason presents this as a float instead of an int (or a
	/// string), so here we are...
	fn try_from(n: &f64) -> ErrResult<Self> {
		match *n as isize {
			0 => Ok(Self::Warning),
			1 => Ok(Self::Critical),
			2 => Ok(Self::PerformanceLoss),
			3 => Ok(Self::SoftShutdown),
			4 => Ok(Self::HardShutdown),
			_ => {
				Err(err_invalid_data("Threshold Severity must be in [0..4]"))
			},
		}
	}
}

/// An array of `T` indexed by [`ThresholdSeverity`].
type ThresholdSeverityArray<T> = [T; ThresholdSeverity::COUNT];

/// The direction of a threshold bound.
#[derive(Debug, Copy, Clone, EnumIter, EnumCount)]
pub enum ThresholdBoundType {
	/// The threshold bound represent a value the sensor reading should not drop below.
	Lower = 0,
	/// The threshold bound represent a value the sensor reading should not exceed.
	Upper = 1,
}

impl ThresholdBoundType {
	/// Return a string representing the bound type.
	fn direction_tag(&self) -> &'static str {
		match self {
			Self::Lower => "Low",
			Self::Upper => "High",
		}
	}
}

impl TryFrom<&String> for ThresholdBoundType {
	type Error = Box<dyn std::error::Error>;
	/// Construct a [`ThresholdBoundType`] from its dbus string representation.
	fn try_from(s: &String) -> ErrResult<Self> {
		match s.as_str() {
			"less than" => Ok(Self::Lower),
			"greater than" => Ok(Self::Upper),
			_ => Err(err_invalid_data("Threshold Direction must be \"less than\" or \
			                           \"greater than\"")),
		}
	}
}

/// An array of `T` indexed by [`ThresholdBoundType`].
type ThresholdBoundTypeArray<T> = [T; ThresholdBoundType::COUNT];

/// Internal representation of a threshold's config data retrieved from dbus.
#[derive(Debug, Copy, Clone)]
pub struct ThresholdConfig {
	// E-M config also includes a "Name" field, but it doesn't
	// appear to be used for anything that I can see...
	kind: ThresholdBoundType,
	severity: ThresholdSeverity,
	/// How much the reading needs to recede from `value` after passing it in order to
	/// reset the alarm status.
	hysteresis: f64,
	value: f64,
	index: Option<usize>,
}

impl ThresholdConfig {
	/// Construct a [`ThresholdConfig`] from a set of dbus properties.
	fn from_dbus(props: &dbus::arg::PropMap) -> ErrResult<Self> {
		let kind = prop_get_mandatory_from(props, "Direction")?;
		let severity = prop_get_mandatory_from(props, "Severity")?;
		let value = *prop_get_mandatory::<f64>(props, "Value")?;
		let hysteresis = *prop_get_default::<f64>(props, "Hysteresis", &0.0f64)?;

		// Index is, of course, naturally an (unsigned) integer, but for some
		// screwball reason it's on dbus as a float (dbus type 'd').  To make sure
		// it isn't something too wacky, ensure its float value is exactly equal
		// to the integer we're turning it into.
		let index = match prop_get_optional::<f64>(props, "Index")?.copied() {
			Some(f) => {
				if !f.is_finite() || f < 0.0 || f != (f as usize) as f64 {
					return Err(err_invalid_data(format!("Invalid 'Index' value {}", f)));
				}
				Some(f as usize)
			},
			None => None,
		};

		if !value.is_finite() {
			return Err(err_invalid_data("Threshold value must be finite"));
		}

		Ok(Self {
			kind,
			severity,
			value,
			hysteresis,
			index,
		})
	}
}

/// Retrieve as many [`ThresholdConfig`]s as are present in `intfs` for the given base
/// interface `baseintf`.
pub fn get_configs_from_dbus(baseintf: &str, intfs: &HashMap<String, dbus::arg::PropMap>)
                             -> Vec<ThresholdConfig>
{
	let mut thresholds = vec![];
	for tidx in 0.. {
		let intf = format!("{}.Thresholds{}", baseintf, tidx);
		let Some(props) = intfs.get(&intf) else {
			break;
		};
		match ThresholdConfig::from_dbus(props) {
			Ok(thr) => thresholds.push(thr),
			Err(e) => {
				error!("Invalid threshold config {}: {}", intf, e);
			},
		}
	}
	thresholds
}

/// Represents a single direction of a threshold.
pub struct ThresholdBound {
	/// The threshold value.
	value: SignalProp<f64>,
	/// See [`ThresholdConfig::hysteresis`].
	hysteresis: f64,
	/// Whether or not an alarm for the threshold is currently asserted.
	alarm: SignalProp<bool>,
}

impl ThresholdBound {
	/// Construct a [`ThresholdBound`] for an object at `dbuspath` using the given
	/// `msgfns` for generating `PropertiesChanged` signal messages and sending them
	/// via `conn`.
	fn new(msgfns: &ThresholdBoundIntfMsgFns, dbuspath: &Arc<SensorPath>,
	       conn: &Arc<SyncConnection>) -> Self {
		Self {
			value: SignalProp::new(f64::NAN, &msgfns.value, dbuspath, conn),
			hysteresis: f64::NAN,
			alarm: SignalProp::new(false, &msgfns.alarm, dbuspath, conn),
		}
	}

	/// Update `self`'s alarm state based on the given direction (`kind`) and `sample`
	/// value from the sensor.
	fn update(&mut self, kind: ThresholdBoundType, sample: f64) {
		if !sample.is_finite() || !self.value.get().is_finite() {
			self.alarm.set(false);
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

/// A threshold of a single severity (lower and upper bounds).
pub struct Threshold {
	/// The lower and upper bounds for a single severity level.
	bounds: ThresholdBoundTypeArray<ThresholdBound>,
}

impl Threshold {
	/// Update both the lower and upper bounds based on the given `sample` value from
	/// the sensor.
	pub fn update(&mut self, sample: f64) {
		for t in ThresholdBoundType::iter() {
			self.bounds[t as usize].update(t, sample);
		}
	}
}

/// A per-severity-level array of threhsolds, as [`Option`]s because they may not have
/// been specified in the available config data.
pub type ThresholdArr = ThresholdSeverityArray<Option<Threshold>>;

/// Construct a [`ThresholdArr`] from a slice of config objects.
pub fn get_thresholds_from_configs(cfgs: &[ThresholdConfig], threshold_intfs: &ThresholdIntfDataArr,
                                   dbuspath: &Arc<SensorPath>, conn: &Arc<SyncConnection>)
                                   -> ThresholdArr
{
	let mut thresholds = ThresholdArr::default();
	for cfg in cfgs {
		let intf = &threshold_intfs[cfg.severity as usize];
		let Ok(bounds) = ThresholdBoundType::iter()
			.map(|t| ThresholdBound::new(&intf.msgfns.bounds[t as usize], dbuspath, conn))
			.collect::<Vec<_>>()
			.try_into() else {
				panic!("ThresholdBoundType::iter() produced wrong number of elements?");
			};
		let threshold = thresholds[cfg.severity as usize].get_or_insert(Threshold { bounds });

		let bound = &mut threshold.bounds[cfg.kind as usize];

		let check = |old: f64, new: f64, name| {
			if !new.is_finite() {
				return false;
			}
			if old.is_finite() {
				warn!("Multiple {:?} {:?} bound {} values specified, \
				       overriding {} with {}", cfg.severity, cfg.kind, name,
				      old, new);
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

/// Retrieve a threshold property value from the given `sensor` for the given severity
/// `sev` via `getter`, sending it back over dbus via `ctx`.
fn get_prop_value<F, R>(mut ctx: dbus_crossroads::PropContext, sensor: &Arc<Mutex<Sensor>>,
                        sev: ThresholdSeverity, getter: F)
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

/// [`PropChgMsgFn`]s for a threshold bound's `value` and `alarm` properties.
struct ThresholdBoundIntfMsgFns {
	value: Arc<PropChgMsgFn>,
	alarm: Arc<PropChgMsgFn>,
}

impl ThresholdBoundIntfMsgFns {
	/// Construct a [`ThresholdBoundIntfMsgFns`] for the given severity `sev` and bound type `kind`.
	fn build(b: &mut IfaceBuilder<Arc<Mutex<Sensor>>>, sev: ThresholdSeverity,
	         kind: ThresholdBoundType) -> Self
	{
		let value = b.property(format!("{}{}", sev.to_str(), kind.direction_tag()))
			.get_async(move |ctx, s| {
				get_prop_value(ctx, s, sev,
				               move |t| t.bounds[kind as usize].value.get())
			})
			.emits_changed_true()
			.changed_msg_fn()
			.into();

		let alarm = b.property(format!("{}Alarm{}", sev.to_str(), kind.direction_tag()))
			.get_async(move |ctx, s| {
				get_prop_value(ctx, s, sev,
				               move |t| t.bounds[kind as usize].alarm.get())
			})
			.emits_changed_true()
			.changed_msg_fn()
			.into();

		Self {
			value,
			alarm,
		}
	}
}

/// The [`PropChgMsgFn`]s for the `xyz.openbmc_project.Sensor.Threshold.*` interfaces.
pub struct ThresholdIntfMsgFns {
	bounds: ThresholdBoundTypeArray<ThresholdBoundIntfMsgFns>,
}

impl ThresholdIntfMsgFns {
	/// Construct a threshold interface for the given severity `sev`.
	fn build(cr: &mut Crossroads, sev: ThresholdSeverity) -> SensorIntf<Self> {
		let intfname = format!("xyz.openbmc_project.Sensor.Threshold.{}", sev.to_str());
		SensorIntf::build(cr, intfname, |b| {
			let Ok(bounds) = ThresholdBoundType::iter()
				.map(|t| ThresholdBoundIntfMsgFns::build(b, sev, t))
				.collect::<Vec<_>>()
				.try_into() else {
					panic!("ThresholdBoundType::iter() produced wrong number \
					        of elements?");
				};
			Self { bounds }
		})
	}
}

/// An per-severity-level array of threshold interfaces, each of which is a
/// [`ThresholdIntfMsgFns`] plus a dbus interface token.
pub type ThresholdIntfDataArr = ThresholdSeverityArray<SensorIntf<ThresholdIntfMsgFns>>;

/// Construct threshold interfaces for all severity levels.
pub fn build_sensor_threshold_intfs(cr: &mut Crossroads) -> ThresholdIntfDataArr {
	let res = ThresholdSeverity::iter()
		.map(|sev| ThresholdIntfMsgFns::build(cr, sev))
		.collect::<Vec<_>>()
		.try_into();

	// .unwrap() unfortunately requires Debug, so do it manually...
	let Ok(arr) = res else {
		panic!("ThresholdSeverity::iter() produced the wrong number of elements?");
	};
	arr
}
