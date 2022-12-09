use std::{sync::Arc, time::Duration};
use dbus::arg::{Variant, RefArg};
use gpiocdev;

use crate::types::ErrResult;

#[derive(Debug, Copy, Clone)]
enum Polarity {
	ActiveHigh,
	ActiveLow,
}

impl Polarity {
	pub fn from_dbus(prop: Option<&Variant<Box<dyn RefArg>>>) -> Option<Self> {
		let Some(var) = prop else {
			return Some(Self::ActiveHigh);
		};
		match var.as_str() {
			Some("High") => Some(Self::ActiveHigh),
			Some("Low") => Some(Self::ActiveLow),
			_ => None,
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
	// FIXME: this (slash its caller(s)) should reject
	// present-but-invalid config instead of just ignoring it
	pub fn from_dbus(cfg: &dbus::arg::PropMap) -> Option<Self> {
		use dbus::arg::prop_cast;
		let name: &String = prop_cast(cfg, "Name")?;
		let setup_sec: f64 = prop_cast(cfg, "SetupTime").copied().unwrap_or(0.0);
		let polarity = Polarity::from_dbus(cfg.get("Polarity"))?;
		Some(Self {
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
			return Err(Box::new(std::io::Error::new(std::io::ErrorKind::NotFound,
								"GPIO not found")));
		};

		// clunk...there's *got* to be a better way of achieving this
		fn set_sense<'a>(builder: &'a mut gpiocdev::request::Builder, pol: Polarity) -> &'a mut gpiocdev::request::Builder {
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
