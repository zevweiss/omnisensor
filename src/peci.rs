//! Backend providing support for Intel PECI sensors.
//!
//! A la dbus-sensors's `intelcpusensor` daemon.

use std::{
	collections::{HashMap, HashSet},
	ops::Deref,
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
		SensorMode::ReadOnly,
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
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str,
	                 intfs: &HashMap<String, dbus::arg::PropMap>) -> ErrResult<Self> {
		let name: &String = prop_get_mandatory(basecfg, "Name")?;
		let cpuid: u64 = *prop_get_mandatory(basecfg, "CpuID")?;
		let bus: u64 = *prop_get_mandatory(basecfg, "Bus")?;
		let address: u64 = *prop_get_mandatory(basecfg, "Address")?;
		let dts_crit_offset = *prop_get_default(basecfg, "DtsCritOffset", &0.0f64)?;

		let thresholds = threshold::get_configs_from_dbus(baseintf, intfs);

		if !dts_crit_offset.is_finite() {
			let msg = format!("{}: DtsCritOffset must be finite (got {})", name,
			                  dts_crit_offset);
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

async fn instantiate_sensor(daemonstate: &DaemonState, path: &InventoryPath,
                            file: sysfs::HwmonFileInfo, cfg: &PECISensorConfig) -> ErrResult<()>
{

	let label = match file.get_label() {
		Ok(s) => s,
		Err(e) => {
			let msg = format!("error finding label for {}: {}",
			                  file.abspath.display(), e);
			return Err(err_invalid_data(msg));
		},
	};

	match label.as_str() {
		"Tcontrol" | "Tthrottle" | "Tjmax" => return Ok(()),
		_ => {},
	}

	let name = format!("{} CPU{}", label, cfg.cpuid);

	let mut sensors = daemonstate.sensors.lock().await;

	let Some(entry) = sensor::get_nonactive_sensor_entry(&mut sensors,
							     name.clone()).await else {
		return Ok(());
	};

	let io = match sysfs::SysfsSensorIO::new(&file).await {
		Ok(io) => sensor::SensorIO::Sysfs(io),
		Err(e) => {
			let msg = format!("error opening {}: {}",
			                  file.abspath.display(), e);
				return Err(err_other(msg));
		},
	};

	let io = SensorIOCtx::new(io);

	let ctor = || {
		Sensor::new(path, &name, file.kind, &daemonstate.sensor_intfs,
			    &daemonstate.bus, ReadOnly)
			.with_power_state(PowerState::On) // FIXME: make configurable?
			.with_thresholds_from(&cfg.thresholds,
					      &daemonstate.sensor_intfs.thresholds,
					      &daemonstate.bus)
			.with_minval(-128.0)
			.with_maxval(127.0)
	};
	sensor::install_or_activate(entry, &daemonstate.crossroads, io,
				    &daemonstate.sensor_intfs, ctor).await;
	Ok(())
}

/// Instantiate any active PECI sensors configured in `daemonstate.config`.
pub async fn instantiate_sensors(daemonstate: &DaemonState, dbuspaths: &FilterSet<InventoryPath>,
                                 retry: &mut HashSet<InventoryPath>) -> ErrResult<()>
{
	let cfgmap = daemonstate.config.lock().await;
	let configs = cfgmap.iter()
		.filter_map(|(path, cfg)| {
			match cfg {
				SensorConfig::PECI(c) if dbuspaths.contains(path) => Some((path, c)),
				_ => None,
			}
		});

	// Doing this unconditionally on every update call is perhaps
	// a little heavy-handed; we could maybe relax things to
	// trigger it from the power signal handler instead.
	if let Err(e) = rescan() {
		eprintln!("Warning: PECI rescan failed: {}", e);
	}

	for (path, pecicfg) in configs {
		let devname = format!("{}-{:02x}", pecicfg.bus, pecicfg.address);
		let devdir = format!("{}/devices/{}", PECI_BUS_DIR, devname);

		// It seems that the PECI interface isn't always immediately
		// ready right after host power-on, and sometimes our rescan
		// attempt hits a little to soon to bring the device online, so
		// check if devdir is there before trying to proceed (and add it
		// to the retry set if it's not).
		if !Path::new(&devdir).exists() {
			eprintln!("Skipping PECI device {}: no {} directory found", devname, devdir);
			retry.insert(path.deref().clone());
			continue;
		}

		for temptype in &["cputemp", "dimmtemp"] {
			// Bleh...the only thing we don't know in advance here is the
			// CPU family (hsx, skx, icx, etc.) matched by the '*'.  Is
			// there some better way of finding this path?
			let sysfs_dir_pat = format!("{}/peci_cpu.{}.*.{}", devdir, temptype,
			                            pecicfg.address);

			let typedir = match sysfs::get_single_glob_match(&sysfs_dir_pat) {
				Ok(d) => d,
				Err(e) => {
					eprintln!("No {} subdirectory for PECI device {}: {}",
					          temptype, devname, e);
					continue;
				},
			};

			let inputs = match sysfs::scan_hwmon_input_files(&typedir, Some("temp")) {
				Ok(v) => v,
				Err(e) => {
					eprintln!("Error finding input files in {}: {}",
					          typedir.display(), e);
					continue;
				},
			};

			for file in inputs {
				if let Err(e) = instantiate_sensor(daemonstate, path,
				                                   file, &pecicfg).await {
					eprintln!("{}: skipping {} entry: {}", path.0,
					          pecicfg.name, e);
				}
			}
		}
	}

	Ok(())
}

/// Whether or not the given `cfgtype` is supported by the `peci` sensor backend.
pub fn match_cfgtype(cfgtype: &str) -> bool {
	cfgtype == "XeonCPU"
}
