//! Backend providing support for Intel PECI sensors.
//!
//! A la dbus-sensors's `intelcpusensor` daemon.

use std::{
	collections::HashMap,
	path::Path,
};

use crate::{
	DaemonState,
	powerstate::PowerState,
	sensor,
	sensor::{
		Sensor,
		SensorConfig,
		SensorIOCtx,
	},
	threshold,
	sysfs,
	types::*,
	dbus_helpers::props::*,
};

/// Internal representation of the dbus config data for a PECI sensor.
///
/// Aside from what's here, there's also a `"PresenceGPIO"` key in a few E-M configs:
///
/// ```
/// "PresenceGPIO": [{ "Name": "...", "Polarity": "..."}]
/// ```
///
/// It seems to be ignored by `intelcpusensor` as far as I can tell though, so we're
/// ignoring it here too (for now at least).  (Also, it's not clear to me why the object
/// is wrapped in an array.)
#[derive(Debug)]
pub struct PECISensorConfig {
	/// Name of the sensor.
	///
	/// FIXME: Is this used for anything?  Need to figure this out.
	name: String,
	/// The ID number of the CPU.
	///
	/// FIXME: Is this used for anything?  Need to figure this out.
	cpuid: u64,
	/// The PECI bus number of the CPU.
	bus: u64,
	/// The PECI address of the CPU.
	address: u64,
	/// The DTS critical offset if provided, 0.0 by default.
	///
	/// This is used as a threshold hysteresis value that optionally overrides what
	/// the kernel provides.
	dts_crit_offset: f64,
	/// Threshold settings for the sensor.
	///
	/// At least in `intelcpusensor`, the threshold information in sysfs is ignored if
	/// any of these are provided.
	thresholds: Vec<threshold::ThresholdConfig>,
}

/// The top-level sysfs directory via which we interact with the kernel's PECI subsystem.
const PECI_BUS_DIR: &str = "/sys/bus/peci";

/// The kernel PECI subsystem doesn't expose a sysfs interface to instantiate a device for
/// a given bus/address; it simply allows triggering a global rescan operation, so this
/// isn't tied to any particular device.
fn rescan() -> ErrResult<()> {
	Ok(std::fs::write(Path::new(PECI_BUS_DIR).join("rescan"), "1")?)
}

impl PECISensorConfig {
	/// Construct a [`PECISensorConfig`] from raw dbus data.
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str, intfs: &HashMap<String, dbus::arg::PropMap>) -> ErrResult<Self> {
		let name: &String = prop_get_mandatory(basecfg, "Name")?;
		let cpuid: u64 = *prop_get_mandatory(basecfg, "CpuID")?;
		let bus: u64 = *prop_get_mandatory(basecfg, "Bus")?;
		let address: u64 = *prop_get_mandatory(basecfg, "Address")?;
		let dts_crit_offset = *prop_get_default(basecfg, "DtsCritOffset", &0.0f64)?;

		let thresholds = threshold::get_configs_from_dbus(baseintf, intfs);

		if !dts_crit_offset.is_finite() {
			let msg = format!("{}: DtsCritOffset must be finite (got {})", name, dts_crit_offset);
			return Err(err_invalid_data(msg));
		}

		Ok(Self {
			name: name.clone(),
			cpuid,
			bus,
			address,
			dts_crit_offset,
			thresholds,
		})
	}
}

/// Instantiate any active PECI sensors configured in `cfgmap`.
pub async fn instantiate_sensors(daemonstate: &DaemonState, dbuspaths: &FilterSet<InventoryPath>) -> ErrResult<()> {
	let cfgmap = daemonstate.config.lock().await;
	let configs = cfgmap.iter()
		.filter_map(|(path, cfg)| {
			match cfg {
				SensorConfig::PECI(pecicfg) if dbuspaths.contains(path) => Some(pecicfg),
				_ => None,
			}
		});

	// Doing this unconditionally on every update call is perhaps
	// a little heavy-handed; we could maybe relax things to
	// trigger it from the power signal handler instead.
	if let Err(e) = rescan() {
		eprintln!("Warning: PECI rescan failed: {}", e);
	}

	for pecicfg in configs {
		// Bleh...the only thing we don't know in advance here is the
		// CPU family (hsx, skx, icx, etc.) matched by the '*'.  Is
		// there some better way of finding this path?
		let devname = format!("{}-{:02x}", pecicfg.bus, pecicfg.address);
		let sysfs_dir_pat = format!("{}/devices/{}/peci_cpu.cputemp.*.{}", PECI_BUS_DIR,
					    devname, pecicfg.address);
		let devdir = match sysfs::get_single_glob_match(&sysfs_dir_pat) {
			Ok(d) => d,
			Err(e) => {
				eprintln!("Failed to find cputemp subdirectory for PECI device {}: {}", devname, e);
				continue;
			},
		};

		let inputs = match sysfs::scan_hwmon_input_files(&devdir, Some("temp")) {
			Ok(v) => v,
			Err(e) => {
				eprintln!("Error finding input files in {}: {}", devdir.display(), e);
				continue;
			},
		};

		for file in inputs {
			let label = match file.get_label() {
				Ok(s) => s,
				Err(e) => {
					eprintln!("{}: error finding label for {}, skipping entry: {}", pecicfg.name,
						  file.abspath.display(), e);
					continue;
				},
			};

			match label.as_str() {
				"Tcontrol" | "Tthrottle" | "Tjmax" => continue,
				_ => {},
			}

			let name = format!("{} {}", label, pecicfg.name);

			let mut sensors = daemonstate.sensors.lock().await;

			let Some(entry) = sensor::get_nonactive_sensor_entry(&mut sensors, name.clone()).await else {
				continue;
			};

			let io = match sysfs::SysfsSensorIO::new(&file).await {
				Ok(io) => io,
				Err(e) => {
					eprintln!("{}: skipping {}: {}", pecicfg.name, file.abspath.display(), e);
					continue;
				},
			};

			let io = SensorIOCtx::new(io);

			sensor::install_or_activate(entry, &daemonstate.crossroads, io, &daemonstate.sensor_intfs, || {
				Sensor::new(&name, file.kind, &daemonstate.sensor_intfs, &daemonstate.bus)
					.with_power_state(PowerState::On) // FIXME: make configurable?
					.with_thresholds_from(&pecicfg.thresholds, &daemonstate.sensor_intfs.thresholds, &daemonstate.bus)
					.with_minval(-128.0)
					.with_maxval(127.0)
			}).await;
		}
	}

	Ok(())
}
