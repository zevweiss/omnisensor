use std::{
	collections::HashMap,
	path::{Path, PathBuf},
	sync::Arc,
};
use dbus::arg::{Variant, RefArg};
use phf::phf_map;

use crate::types::ErrResult;

// Functionally this could be simplified to a set (perhaps even with
// membership implying false instead of true to keep it smaller), but
// listing all known devices explicitly seems preferable
// maintainability-wise.
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

// FIXME: Clone on this is kind of ugly...
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct I2CDeviceParams {
	pub bus: u16,
	pub address: u16,
	pub devtype: String,
}

const I2C_DEV_DIR: &'static str = "/sys/bus/i2c/devices";

impl I2CDeviceParams {
	pub fn from_dbus(cfg: &dbus::arg::PropMap, r#type: &str) -> ErrResult<Option<Self>> {
		let bus = cfg.get("Bus");
		let address = cfg.get("Address");
		let (bus, address) = match (bus, address) {
			(Some(b), Some(a)) => (b, a),
			(None, None) => return Ok(None),
			_ => return Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData,
								     "Bus and Address must both be specified"))),
		};
		let get_u16 = |v: &Variant<Box<dyn RefArg>>| -> ErrResult<u16> {
			let x = match v.as_u64() {
				Some(x) => x,
				_ => return Err(Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData,
									     "Bus and Address must both be u64"))),
			};
			Ok(x.try_into()?)
		};
		Ok(Some(Self {
			bus: get_u16(bus)?,
			address: get_u16(address)?,
			devtype: r#type.into(),
		}))
	}

	pub fn sysfs_name(&self) -> String {
		format!("{}-{:04x}", self.bus, self.address)
	}

	pub fn sysfs_device_dir(&self) -> String {
		format!("{}/{}", I2C_DEV_DIR, self.sysfs_name())
	}

	pub fn sysfs_bus_dir(&self) -> String {
		format!("{}/i2c-{}", I2C_DEV_DIR, self.bus)
	}

	pub fn device_present(&self) -> bool {
		let mut path = PathBuf::from(&self.sysfs_device_dir());
		if *I2C_HWMON_DEVICES.get(&self.devtype).unwrap_or(&true) {
			path.push("hwmon");
		}
		path.exists()
	}

	pub fn device_static(&self) -> bool {
		if !self.device_present() {
			false
		} else {
			Path::new(&self.sysfs_device_dir()).join("of_node").exists()
		}
	}

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

pub struct I2CDevice {
	pub params: I2CDeviceParams,
}

impl I2CDevice {
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
			Err(Box::new(std::io::Error::new(std::io::ErrorKind::Other,
							 "new_device failed to instantiate device")))
		}
	}
}

impl Drop for I2CDevice {
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

// Lookup table for finding I2CDevices.  Weak<_> because we only want
// them kept alive by (strong, Arc<_>) references from Sensors so they
// get dropped when the last sensor using them goes away.
pub type I2CDeviceMap = HashMap<I2CDeviceParams, std::sync::Weak<I2CDevice>>;

// Find an existing I2CDevice in devmap for the given params, or
// instantiate one and add it to devmap if not.
pub fn get_i2cdev(devmap: &mut I2CDeviceMap, params: &I2CDeviceParams) -> ErrResult<Option<Arc<I2CDevice>>> {
	let d = devmap.get(params).map(|w| w.upgrade()).flatten();
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
