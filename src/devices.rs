//! Abstractions for managing physical devices.

use std::collections::HashMap;

/// An object representing a dynamically-managed underlying device providing one
/// or more sensors.
#[allow(clippy::upper_case_acronyms)]
pub enum PhysicalDevice {
	/// An I2C device, managed via the `new_device` and
	/// `delete_device` operations of its parent bus.
	I2C(i2c::I2CDevice),

	/// A PECI device, managed via the (global) `rescan` and
	/// (per-device) `remove` operations.
	#[cfg(feature = "peci")]
	PECI(peci::PECIDevice),
}

pub mod i2c;

#[cfg(feature = "peci")]
pub mod peci;

/// Lookup table for finding physical devices.  Weak<_> because we only want them kept
/// alive by (strong, Arc<_>) references from Sensors so they get dropped when the last
/// sensor using them goes away.
pub struct PhysicalDeviceMap {
	/// All dynamic I2C devices.
	i2c: HashMap<i2c::I2CDeviceParams, std::sync::Weak<PhysicalDevice>>,

	/// PECI devices (which are always dynamic).
	#[cfg(feature = "peci")]
	peci: HashMap<peci::PECIDeviceParams, std::sync::Weak<PhysicalDevice>>,
}

impl PhysicalDeviceMap {
	/// Construct a new [`PhysicalDeviceMap`].
	pub fn new() -> Self {
		Self {
			i2c: HashMap::new(),

			#[cfg(feature = "peci")]
			peci: HashMap::new(),
		}
	}
}
