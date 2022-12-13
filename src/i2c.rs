//! Abstractions for managing I2C devices.

use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::Arc,
};
use phf::phf_map;

use crate::{
	types::*,
	dbus_helpers::props::*,
};

/// A map of supported I2C device types.
///
/// Keys are supported I2C device types (in capitalized dbus-config-data form, not the
/// lowercase form used by the kernel) and whose values are `bool`s indicating whether or
/// not the device is expected to create a `hwmon` directory when instantiated.
///
/// Functionally this could be simplified to a set (perhaps even with membership implying
/// false instead of true to keep it smaller), but listing all known devices explicitly
/// seems preferable maintainability-wise.
static I2C_HWMON_DEVICES: phf::Map<&'static str, bool> = phf_map! {
	"DPS310"   => false,
	"EMC1412"  => true,
	"EMC1413"  => true,
	"EMC1414"  => true,
	"HDC1080"  => false,
	"JC42"     => true,
	"LM75A"    => true,
	"LM95234"  => true,
	"MAX31725" => true,
	"MAX31730" => true,
	"MAX6581"  => true,
	"MAX6654"  => true,
	"NCT6779"  => true,
	"NCT7802"  => true,
	"SBTSI"    => true,
	"SI7020"   => false,
	"TMP112"   => true,
	"TMP175"   => true,
	"TMP421"   => true,
	"TMP441"   => true,
	"TMP75"    => true,
	"W83773G"  => true,
};

/// The information needed to instantiate an I2C device.
// FIXME: Clone on this is kind of ugly...
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct I2CDeviceParams {
	/// I2C bus number.
	pub bus: u16,
	/// I2C address.
	pub address: u16,
	/// Device type, still in capitalized form.  (We downcase it
	/// on the fly before writing it into `new_device`.)
	pub devtype: String,
}

/// The sysfs directory into which all i2c devices are mapped.
const I2C_DEV_DIR: &str = "/sys/bus/i2c/devices";

impl I2CDeviceParams {
	/// Create an [`I2CDeviceParams`] from a set of dbus properties and a type string.
	pub fn from_dbus(cfg: &dbus::arg::PropMap, r#type: &str) -> ErrResult<Self> {
		let bus: u64 = *prop_get_mandatory(cfg, "Bus")?;
		let address: u64 = *prop_get_mandatory(cfg, "Address")?;
		Ok(Self {
			bus: bus.try_into()?,
			address: address.try_into()?,
			devtype: r#type.into(),
		})
	}

	/// Return the device name as employed in sysfs, e.g. `"2-004c"`.
	pub fn sysfs_name(&self) -> String {
		format!("{}-{:04x}", self.bus, self.address)
	}

	/// Return the absolute path of the sysfs directory representing the device.
	pub fn sysfs_device_dir(&self) -> String {
		format!("{}/{}", I2C_DEV_DIR, self.sysfs_name())
	}

	/// Return the absolute path of the sysfs directory representing the bus via which
	/// the device is attached.
	pub fn sysfs_bus_dir(&self) -> String {
		format!("{}/i2c-{}", I2C_DEV_DIR, self.bus)
	}

	/// Test if the device is currently present, i.e. has had a driver successfully
	/// bound to it.
	pub fn device_present(&self) -> bool {
		let mut path = PathBuf::from(&self.sysfs_device_dir());
		if *I2C_HWMON_DEVICES.get(&self.devtype).unwrap_or(&true) {
			path.push("hwmon");
		}
		path.exists()
	}

	/// Test if the device is static, i.e. instantiated from a device-tree node (as
	/// opposed to a dynamic device instantiate by userspace writing to `new_device`).
	pub fn device_static(&self) -> bool {
		if !self.device_present() {
			false
		} else {
			Path::new(&self.sysfs_device_dir()).join("of_node").exists()
		}
	}

	/// Attempt to instantiate the device represented by `self`, returning:
	///
	///  * `Ok(None)` if the device is static (we don't need to manage it).
	///  * `Ok(Some(_))` on success (an [`I2CDevice`] that will remove the device when
	///    the last reference to it is dropped).
	///  * `Err(_)` on error.
	pub fn instantiate_device(&self) -> ErrResult<Option<Arc<I2CDevice>>> {
		if self.device_static() {
			Ok(None)
		} else {
			// There exist error cases in which a sensor device that
			// we need is already instantiated, but needs to be
			// destroyed and re-created in order to be useful (for
			// example if we crash after instantiating a device and
			// the sensor device's power is cut before we get
			// restarted, leaving it "present" but not really
			// usable).  To be on the safe side, instantiate a
			// temporary device and immediately destroy it so as to
			// ensure that we end up with a fresh instance of it.
			if self.device_present() {
				drop(I2CDevice::new(self.clone()));
			}
			I2CDevice::new(self.clone()).map(|d| Some(Arc::new(d)))
		}
	}
}

/// An instantiated I2C device.
///
/// Doesn't do much aside from acting as a token that keeps the device instantiated as
/// long as it exists (its [`drop`](I2CDevice::drop) impl deletes the device).
pub struct I2CDevice {
	/// The parameters of the device we've instantiated.
	pub params: I2CDeviceParams,
}

impl I2CDevice {
	/// Instantiate an [`I2CDevice`] from an [`I2CDeviceParams`].
	pub fn new(params: I2CDeviceParams) -> ErrResult<Self> {
		// If it's already instantiated, there's nothing we need to do.
		let dev = Self { params };
		if dev.params.device_present() {
			return Ok(dev);
		}

		// Try to create it: 'echo $devtype $addr > .../i2c-$bus/new_device'
		let ctor_path = Path::new(&dev.params.sysfs_bus_dir()).join("new_device");
		let payload = format!("{} {:#02x}\n", dev.params.devtype.to_lowercase(), dev.params.address);
		eprintln!(">>> Instantiating {} at {}", dev.params.devtype, dev.params.sysfs_name());
		std::fs::write(&ctor_path, payload)?;

		// Check if that created the requisite sysfs directory
		if dev.params.device_present() {
			Ok(dev)
		} else {
			// ...if not the implicit drop of 'dev' will tear down
			// any partially-instantiated remnants
			Err(err_other("new_device failed to instantiate device"))
		}
	}
}

impl Drop for I2CDevice {
	/// Deletes the I2C device represented by `self` by writing to `delete_device`.
	fn drop(&mut self) {
		eprintln!("<<< Deleting {} at {}", self.params.devtype, self.params.sysfs_name());
		// No params.devicePresent() check on this like in
		// I2CDevice::new(), since it might be used to clean up after a
		// device instantiation that was only partially successful
		// (i.e. when params.device_present() would return false but
		// there's still a dummy i2c client device to remove)
		let dtor_path = Path::new(&self.params.sysfs_bus_dir()).join("delete_device");
		let payload = format!("{:#02x}\n", self.params.address);
		if let Err(e) = std::fs::write(&dtor_path, payload) {
			eprintln!("Failed to write to {}: {}", dtor_path.display(), e);
		}
	}
}

/// Lookup table for finding I2CDevices.  Weak<_> because we only want them kept alive by
/// (strong, Arc<_>) references from Sensors so they get dropped when the last sensor
/// using them goes away.
pub type I2CDeviceMap = HashMap<I2CDeviceParams, std::sync::Weak<I2CDevice>>;

/// Find an existing [`I2CDevice`] in `devmap` for the given `params`, or instantiate one
/// and add it to `devmap` if not.  Returns:
///
///  * `Ok(Some(_))` if an existing device was found or a new one successfully instantiated.
///  * `Ok(None)` if the device is static.
///  * `Err(_)` on error.
pub fn get_i2cdev(devmap: &mut I2CDeviceMap, params: &I2CDeviceParams) -> ErrResult<Option<Arc<I2CDevice>>> {
	let d = devmap.get(params).and_then(|w| w.upgrade());
	if d.is_some() {
		Ok(d)
	} else {
		let dev = params.instantiate_device()?;
		if let Some(ref d) = dev {
			devmap.insert(params.clone(), Arc::downgrade(d));
		}
		Ok(dev)
	}
}
