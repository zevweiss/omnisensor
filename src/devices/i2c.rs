//! Abstractions for managing I2C devices.

use std::{
	path::{Path, PathBuf},
	sync::Arc,
};
use phf::{phf_map, phf_set};
use log::{debug, error};

use crate::{
	types::*,
	dbus_helpers::props::*,
};

use super::{
	PhysicalDevice,
	PhysicalDeviceMap,
};

/// A map of supported I2C device types.
///
/// Keys are supported I2C device types values are `bool`s indicating whether or not the
/// device is expected to create a `hwmon` directory when instantiated.
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
	"TMP464"   => true,
	"TMP75"    => true,
	"W83773G"  => true,
};

/// A set of device types known to be PMBus sensors.
///
/// While broadly similar, PMBus sensors and "basic" hwmon sensors use somewhat different
/// config schemas; this is used to determine which one we're dealing with.
static I2C_PMBUS_TYPES: phf::Set<&'static str> = phf_set! {
	"ADM1266",
	"ADM1272",
	"ADM1275",
	"ADM1278",
	"ADM1293",
	"ADS7830",
	"AHE50DC_FAN",
	"BMR490",
	"CFFPS",
	"CFFPS1",
	"CFFPS2",
	"CFFPS3",
	"DPS800",
	"INA219",
	"INA230",
	"IPSPS1",
	"IR38060",
	"IR38164",
	"IR38263",
	"ISL68137",
	"ISL68220",
	"ISL68223",
	"ISL69225",
	"ISL69243",
	"ISL69260",
	"ISL69269",
	"LM25066",
	"MAX16601",
	"MAX20710",
	"MAX20730",
	"MAX20734",
	"MAX20796",
	"MAX34451",
	"MP2971",
	"MP2973",
	"MP2975",
	"MP5023",
	"NCP4200",
	"PLI1209BC",
	"pmbus",
	"PXE1610",
	"RAA228000",
	"RAA228228",
	"RAA228620",
	"RAA229001",
	"RAA229004",
	"RAA229126",
	"TDA38640",
	"TPS53679",
	"TPS546D24",
	"XDPE11280",
	"XDPE12284",
	"XDPE152C4",
};

/// Retrieve an I2CDeviceType for the given device type string.
pub fn get_device_type(s: &str) -> Option<I2CDeviceType> {
	if let Some(k) = I2C_PMBUS_TYPES.get_key(s) {
		Some(I2CDeviceType::PMBus(PMBusDeviceType { name: k }))
	} else if let Some((k, v)) = I2C_HWMON_DEVICES.get_entry(s) {
		Some(I2CDeviceType::Hwmon(HwmonDeviceType {
			name: k,
			creates_hwmon: *v,
		}))
	} else {
		None
	}
}

/// A basic (non-pmbus) hwmon I2C device type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct HwmonDeviceType {
	/// Device type name (uppercase).
	name: &'static str,
	/// Whether or not we expect the driver to create a `hwmon` subdirectory when
	/// bound to a device.
	creates_hwmon: bool,
}

/// A PMBus I2C device type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PMBusDeviceType {
	/// Device type name (uppercase).
	name: &'static str,
}

/// An enum for an I2C device type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum I2CDeviceType {
	PMBus(PMBusDeviceType),
	Hwmon(HwmonDeviceType),
}

impl I2CDeviceType {
	/// Return the name of the device type.
	pub fn name(&self) -> &'static str {
		match self {
			Self::PMBus(p) => p.name,
			Self::Hwmon(h) => h.name,
		}
	}

	/// Whether or not the device type is expected to create a `hwmon` subdirectory
	/// when a driver is bound.
	pub fn creates_hwmon(&self) -> bool {
		match self {
			// All PMBus devices are expected to, so we don't store it explicitly.
			Self::PMBus(_) => true,
			Self::Hwmon(h) => h.creates_hwmon,
		}
	}

	/// Return the (lower-case) form of the string used to write into `new_device`.
	fn kernel_type(&self) -> String {
		self.name().to_lowercase()
	}
}

/// The location of an I2C device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct I2CLocation {
	/// I2C bus number.
	pub bus: u16,
	/// I2C address.
	pub address: u16,
}

impl I2CLocation {
	/// Create an [`I2CLocation`] from a set of dbus properties.
	pub fn from_dbus(cfg: &dbus::arg::PropMap) -> ErrResult<Self> {
		let bus: u64 = *prop_get_mandatory(cfg, "Bus")?;
		let address: u64 = *prop_get_mandatory(cfg, "Address")?;
		Ok(Self {
			bus: bus.try_into()?,
			address: address.try_into()?,
		})
	}

	/// Return a device name as employed in sysfs, e.g. `"2-004c"`.
	pub fn sysfs_name(&self) -> String {
		format!("{}-{:04x}", self.bus, self.address)
	}

	/// Return the absolute path of the sysfs directory representing a device at this location.
	pub fn sysfs_device_dir(&self) -> PathBuf {
		Path::new(I2C_DEV_DIR).join(self.sysfs_name())
	}

	/// Return the absolute path of the sysfs directory representing the bus for this location.
	pub fn sysfs_bus_dir(&self) -> PathBuf {
		Path::new(I2C_DEV_DIR).join(format!("i2c-{}", self.bus))
	}
}

/// The information needed to instantiate an I2C device.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct I2CDeviceParams {
	/// I2C bus & address.
	pub loc: I2CLocation,
	/// Device type.
	pub devtype: I2CDeviceType,
}

/// The sysfs directory into which all i2c devices are mapped.
const I2C_DEV_DIR: &str = "/sys/bus/i2c/devices";

impl I2CDeviceParams {
	/// Create an [`I2CDeviceParams`] from a set of dbus properties and a type string.
	pub fn from_dbus(cfg: &dbus::arg::PropMap, devtype: I2CDeviceType) -> ErrResult<Self> {
		Ok(Self {
			loc: I2CLocation::from_dbus(cfg)?,
			devtype,
		})
	}

	/// Return the absolute path of the sysfs directory representing the device.
	pub fn sysfs_device_dir(&self) -> PathBuf {
		self.loc.sysfs_device_dir()
	}

	/// Test if the device is currently present, i.e. has had a driver successfully
	/// bound to it.
	pub fn device_present(&self) -> bool {
		let mut path = self.sysfs_device_dir();
		if self.devtype.creates_hwmon() {
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
			self.sysfs_device_dir().join("of_node").exists()
		}
	}

	/// Attempt to instantiate the device represented by `self`, returning:
	///
	///  * `Ok(None)` if the device is static (we don't need to manage it).
	///  * `Ok(Some(_))` on success (an [`I2CDevice`] that will remove the device when
	///    the last reference to it is dropped).
	///  * `Err(_)` on error.
	fn instantiate_device(&self) -> ErrResult<Option<Arc<PhysicalDevice>>> {
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
			let physdev = PhysicalDevice::I2C(I2CDevice::new(self.clone())?);
			Ok(Some(Arc::new(physdev)))
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
	fn new(params: I2CDeviceParams) -> ErrResult<Self> {
		// If it's already instantiated, there's nothing we need to do.
		let dev = Self { params };
		if dev.params.device_present() {
			return Ok(dev);
		}

		let devtype = dev.params.devtype.kernel_type();

		debug!("instantiating {} i2c device {}", devtype, dev.params.loc.sysfs_name());

		// Try to create it: 'echo $devtype $addr > .../i2c-$bus/new_device'
		let ctor_path = dev.params.loc.sysfs_bus_dir().join("new_device");
		let payload = format!("{} {:#02x}\n", devtype, dev.params.loc.address);
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
		debug!("removing i2c device {}", self.params.loc.sysfs_name());

		// No params.devicePresent() check on this like in
		// I2CDevice::new(), since it might be used to clean up after a
		// device instantiation that was only partially successful
		// (i.e. when params.device_present() would return false but
		// there's still a dummy i2c client device to remove)
		let dtor_path = self.params.loc.sysfs_bus_dir().join("delete_device");
		let payload = format!("{:#02x}\n", self.params.loc.address);
		if let Err(e) = std::fs::write(&dtor_path, payload) {
			error!("Failed to write to {}: {}", dtor_path.display(), e);
		}
	}
}

/// Find an existing I2C [`PhysicalDevice`] in `devmap` for the given `params`, or
/// instantiate one and add it to `devmap` if not.  Returns:
///
///  * `Ok(Some(_))` if an existing device was found or a new one successfully instantiated.
///  * `Ok(None)` if the device is static.
///  * `Err(_)` on error.
pub fn get_i2cdev(devmap: &mut PhysicalDeviceMap, params: &I2CDeviceParams)
                  -> ErrResult<Option<Arc<PhysicalDevice>>>
{
	let d = devmap.i2c.get(params).and_then(|w| w.upgrade());
	if d.is_some() {
		Ok(d)
	} else {
		let dev = params.instantiate_device()?;
		if let Some(ref d) = dev {
			devmap.i2c.insert(params.clone(), Arc::downgrade(d));
		}
		Ok(dev)
	}
}
