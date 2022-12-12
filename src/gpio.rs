use std::{sync::Arc, time::Duration};

use crate::{
	dbus_helpers::props::*,
	types::*,
};

#[derive(Debug, Copy, Clone)]
enum Polarity {
	ActiveHigh,
	ActiveLow,
}

impl TryFrom<&str> for Polarity {
	type Error = Box<dyn std::error::Error>;
	fn try_from(s: &str) -> ErrResult<Self> {
		match s {
			"High" => Ok(Self::ActiveHigh),
			"Low" => Ok(Self::ActiveLow),
			_ => Err(err_invalid_data("Polarity must be \"High\" or \"Low\"")),
		}
	}
}

#[derive(Debug)]
pub struct BridgeGPIOConfig {
	pub name: String,
	setup_time: Duration,
	polarity: Polarity,
}

impl BridgeGPIOConfig {
	pub fn from_dbus(cfg: &dbus::arg::PropMap) -> ErrResult<Self> {
		let name: &String = prop_get_mandatory(cfg, "Name")?;
		let setup_sec: f64 = *prop_get_default(cfg, "SetupTime", &0.0f64)?;
		let polarity = prop_get_default_from::<str, _>(cfg, "Polarity", Polarity::ActiveHigh)?;
		Ok(Self {
			name: name.clone(),
			setup_time: Duration::from_secs_f64(setup_sec),
			polarity,
		})
	}
}

pub struct BridgeGPIO {
	line: gpiocdev::FoundLine,
	cfg: Arc<BridgeGPIOConfig>,
	req: gpiocdev::request::Request,
}

pub struct BridgeGPIOActivation<'a> {
	gpio: &'a BridgeGPIO
}

impl BridgeGPIO {
	pub fn from_config(cfg: Arc<BridgeGPIOConfig>) -> ErrResult<Self> {
		let Some(line) = gpiocdev::find_named_line(&cfg.name) else {
			eprintln!("failed to find bridge GPIO {}", cfg.name);
			return Err(err_not_found("GPIO not found"));
		};

		// clunk...there's *got* to be a better way of achieving this
		fn set_sense(builder: &mut gpiocdev::request::Builder, pol: Polarity) -> &mut gpiocdev::request::Builder {
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
		})
	}

	pub async fn activate(&self) -> ErrResult<BridgeGPIOActivation<'_>> {
		self.req.set_value(self.line.offset, gpiocdev::line::Value::Active)?;
		tokio::time::sleep(self.cfg.setup_time).await;
		Ok(BridgeGPIOActivation {
			gpio: self,
		})
	}
}

impl Drop for BridgeGPIOActivation<'_> {
	fn drop(&mut self) {
		if let Err(e) = self.gpio.req.set_value(self.gpio.line.offset, gpiocdev::line::Value::Inactive) {
			eprintln!("failed to reset bridge gpio {}: {}", self.gpio.cfg.name, e);
		}
	}
}
