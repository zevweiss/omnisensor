//! Abstractions for handling GPIO lines associated with sensors.

use std::{sync::Arc, time::Duration};
use log::error;

use crate::{
	dbus_helpers::props::*,
	types::*,
};

/// The polarity of a GPIO.
#[derive(Debug, Copy, Clone)]
enum Polarity {
	ActiveHigh,
	ActiveLow,
}

impl TryFrom<&String> for Polarity {
	type Error = Box<dyn std::error::Error>;
	fn try_from(s: &String) -> ErrResult<Self> {
		match s.as_ref() {
			"High" => Ok(Self::ActiveHigh),
			"Low" => Ok(Self::ActiveLow),
			_ => Err(err_invalid_data("Polarity must be \"High\" or \"Low\"")),
		}
	}
}

/// Internal representation of bridge GPIO config data from dbus.
#[derive(Debug)]
pub struct BridgeGPIOConfig {
	/// The name of the GPIO line.
	pub name: String,
	/// How long to hold the line in its active state before sampling an
	/// associated sensor.
	setup_time: Duration,
	/// The polarity of the GPIO line.
	polarity: Polarity,
}

impl BridgeGPIOConfig {
	/// Construct a [`BridgeGPIOConfig`] from a set of properties retrieved
	/// from dbus.
	pub fn from_dbus(cfg: &dbus::arg::PropMap) -> ErrResult<Self> {
		let name: &String = prop_get_mandatory(cfg, "Name")?;
		let setup_sec: f64 = *prop_get_default(cfg, "SetupTime", &0.0f64)?;
		let polarity = prop_get_default_from(cfg, "Polarity", Polarity::ActiveHigh)?;
		Ok(Self {
			name: name.clone(),
			setup_time: Duration::from_secs_f64(setup_sec),
			polarity,
		})
	}
}

/// Represents a GPIO that must be asserted for a sensor to be sampled.
pub struct BridgeGPIO {
	line: gpiocdev::FoundLine,
	cfg: Arc<BridgeGPIOConfig>,
	req: gpiocdev::request::Request,
	active: bool,
}

/// A guard object representing a [`BridgeGPIO`] held in its active state.
pub struct BridgeGPIOActivation<'a> {
	/// The associated [`BridgeGPIO`]
	gpio: &'a mut BridgeGPIO
}

impl BridgeGPIO {
	/// Construct a [`BridgeGPIO`] from a [`BridgeGPIOConfig`]
	pub fn from_config(cfg: Arc<BridgeGPIOConfig>) -> ErrResult<Self> {
		let Some(line) = gpiocdev::find_named_line(&cfg.name) else {
			error!("failed to find bridge GPIO {}", cfg.name);
			return Err(err_not_found("GPIO not found"));
		};

		// clunk...there's *got* to be a better way of achieving this
		fn set_sense(builder: &mut gpiocdev::request::Builder, pol: Polarity)
		             -> &mut gpiocdev::request::Builder
		{
			match pol {
				Polarity::ActiveHigh => builder.as_active_high(),
				Polarity::ActiveLow => builder.as_active_low(),
			}
		}
		let req = set_sense(gpiocdev::request::Request::builder()
		                    .with_found_line(&line)
		                    .with_direction(gpiocdev::line::Direction::Output),
		                    cfg.polarity)
			.request()?;

		Ok(Self {
			line,
			cfg,
			req,
			active: false,
		})
	}

	/// Sets the GPIO to its activate state and returns a
	/// [`BridgeGPIOActivation`] guard object that will reset it when
	/// [`drop`](BridgeGPIOActivation::drop)ped.
	pub async fn activate(&mut self) -> ErrResult<BridgeGPIOActivation<'_>> {
		if self.active {
			return Err(err_other("GPIO activation already held"));
		}
		self.req.set_value(self.line.offset, gpiocdev::line::Value::Active)?;
		tokio::time::sleep(self.cfg.setup_time).await;
		self.active = true;
		Ok(BridgeGPIOActivation {
			gpio: self,
		})
	}
}

impl Drop for BridgeGPIOActivation<'_> {
	/// Resets the associated [`BridgeGPIO`] back to its inactive state.
	fn drop(&mut self) {
		if let Err(e) = self.gpio.req.set_value(self.gpio.line.offset,
		                                        gpiocdev::line::Value::Inactive) {
			error!("failed to reset bridge gpio {}: {}", self.gpio.cfg.name, e);
		}
		self.gpio.active = false;
	}
}
