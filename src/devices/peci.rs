
use std::{
	path::{Path, PathBuf},
	sync::Arc,
};
use log::error;

use crate::{
	dbus_helpers::props::*,
	types::*,
};

use super::{
	PhysicalDevice,
	PhysicalDeviceMap,
};

/// The top-level sysfs directory via which we interact with the kernel's PECI subsystem.
const PECI_BUS_DIR: &str = "/sys/bus/peci";

/// The kernel PECI subsystem doesn't expose a sysfs interface to instantiate a device for
/// a given bus/address; it simply allows triggering a global rescan operation, so this
/// isn't tied to any particular device.
// FIXME: make async?  (can be slow if host is off)
fn rescan() -> ErrResult<()> {
	Ok(std::fs::write(Path::new(PECI_BUS_DIR).join("rescan"), "1")?)
}

/// The information needed to instantiate a PECI device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PECIDeviceParams {
	/// PECI bus number.
	pub bus: u16,
	/// PECI address.
	pub address: u8,
}

impl PECIDeviceParams {
	/// Create a [`PECIDeviceParams`] from a set of dbus properties.
	pub fn from_dbus(cfg: &dbus::arg::PropMap) -> ErrResult<Self> {
		let bus: u64 = *prop_get_mandatory(cfg, "Bus")?;
		let address: u64 = *prop_get_mandatory(cfg, "Address")?;
		Ok(Self {
			bus: bus.try_into()?,
			address: address.try_into()?,
		})
	}

	/// Return the device name as employed in sysfs, e.g. `"0-30"`.
	pub fn sysfs_name(&self) -> String {
		format!("{}-{:02x}", self.bus, self.address)
	}

	/// Return the absolute path of the sysfs directory representing the device.
	pub fn sysfs_device_dir(&self) -> PathBuf {
		Path::new(PECI_BUS_DIR).join("devices").join(self.sysfs_name())
	}

	/// Test if the device is currently present, i.e. has been detected by a
	/// [`rescan`] and not subsequently removed.
	pub fn device_present(&self) -> bool {
		self.sysfs_device_dir().exists()
	}

	/// Attempt to instantiate the device represented by `self`; an Ok(_)
	/// return produces a [`PECIDevice`] that will remove the device when the
	/// last reference to it is dropped.
	fn instantiate_device(&self) -> ErrResult<Arc<PhysicalDevice>> {
		// Ensure we're not dealing with a stale device left over after a
		// crash (which we could probably actually get away with for PECI
		// since there isn't much state to speak of maintained in the
		// driver [or the device for that matter], but for the sake of
		// consistency and clean slates...)
		if self.device_present() {
			drop(PECIDevice::new(self.clone()));
		}
		let physdev = PhysicalDevice::PECI(PECIDevice::new(self.clone())?);
		Ok(Arc::new(physdev))
	}
}

/// An instantiated PECI device.
///
/// Doesn't do much aside from acting as a token that keeps the device instantiated as
/// long as it exists (its [`drop`](PECIDevice::drop) impl deletes the device).
pub struct PECIDevice {
	/// The parameters of the device we've instantiated.
	pub params: PECIDeviceParams,
}

impl PECIDevice {
	/// Construct a [`PECIDevice`] from a [`PECIDeviceParams`].
	fn new(params: PECIDeviceParams) ->ErrResult<Self> {
		let dev = Self { params };
		if dev.params.device_present() {
			return Ok(dev);
		}

		// Even if rescan() fails there's still some chance it may have
		// found the device we're concerned with before doing so, so
		// continue on and check for it even on error...
		let rescan_err = rescan().err();

		if dev.params.device_present() {
			Ok(dev)
		} else {
			match rescan_err {
				// Report the rescan error if there was one
				Some(e) => Err(e),

				// Otherwise a generic error message
				None => {
					let msg = format!("{} not found after PECI rescan",
					                  dev.params.sysfs_name());
					Err(err_not_found(msg))
				},
			}
		}
	}
}

impl Drop for PECIDevice {
	/// Deletes the PECI device represented by `self` by writing to its `remove` file.
	fn drop(&mut self) {
		let dtor_path = self.params.sysfs_device_dir().join("remove");
		if let Err(e) = std::fs::write(&dtor_path, "1") {
			error!("Failed to write to {}: {}", dtor_path.display(), e);
		}
	}
}

/// Find an existing PECI [`PhysicalDevice`] in `devmap` for the given `params`, or
/// instantiate one and add it to `devmap` if not.
#[cfg(feature = "peci")]
pub fn get_pecidev(devmap: &mut PhysicalDeviceMap, params: &PECIDeviceParams)
                   -> ErrResult<Arc<PhysicalDevice>>
{
	match devmap.peci.get(params).and_then(|w| w.upgrade()) {
		Some(d) => Ok(d),
		_ => {
			let dev = params.instantiate_device()?;
			devmap.peci.insert(params.clone(), Arc::downgrade(&dev));
			Ok(dev)
		}
	}
}
