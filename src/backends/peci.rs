//! Backend providing support for Intel PECI sensors.
//!
//! A la dbus-sensors's `intelcpusensor` daemon.

use std::{
	collections::{HashMap, HashSet},
	ops::Deref,
	sync::Arc,
};
use log::error;

use crate::{
	DaemonState,
	devices::{
		PhysicalDevice,
		peci::{
			PECIDeviceParams,
			get_pecidev,
		},
	},
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
	/// The PECI bus parameters (bus number and address) of the CPU.
	params: PECIDeviceParams,
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

impl PECISensorConfig {
	/// Construct a [`PECISensorConfig`] from raw dbus data.
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str,
	                 intfs: &HashMap<String, dbus::arg::PropMap>) -> ErrResult<Self> {
		let name: &String = prop_get_mandatory(basecfg, "Name")?;
		let cpuid: u64 = *prop_get_mandatory(basecfg, "CpuID")?;
		let params = PECIDeviceParams::from_dbus(basecfg)?;
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
			params,
			dts_crit_offset,
			thresholds,
		})
	}
}

async fn instantiate_sensor(daemonstate: &DaemonState, path: &InventoryPath,
                            file: sysfs::HwmonFileInfo, cfg: &PECISensorConfig,
                            physdev: &Arc<PhysicalDevice>) -> ErrResult<()>
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

	let io = SensorIOCtx::new(io).with_physdev(Some(physdev.clone()));

	let ctor = || {
		Sensor::new(path, &name, file.kind, &daemonstate.sensor_intfs,
			    &daemonstate.bus, ReadOnly)
			.with_power_state(PowerState::BiosPost)
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

	// If the host's not on, there's not much for PECI to do, so skip the
	// (potentially slow) rescan and just return.  Experimentally, the PECI
	// interface starts responding pretty immediately after power-on, but it
	// takes a while for CPU & DIMM temperature readings to actually become
	// available, so wait for the POST-complete signal before trying anything.
	if !PowerState::BiosPost.active_now() {
		return Ok(());
	}

	for (path, pecicfg) in configs {
		let params = &pecicfg.params;
		let devdir = params.sysfs_device_dir();

		let physdev = {
			let mut physdevs = daemonstate.physdevs.lock().await;
			match get_pecidev(&mut physdevs, params) {
				Ok(d) => d,
				Err(e) => {
					error!("{}: PECI device not found: {}",
					       params.sysfs_name(), e);
					retry.insert(path.deref().clone());
					continue;
				},
			}
		};

		for temptype in &["cputemp", "dimmtemp"] {
			// Bleh...the only thing we don't know in advance here is the
			// CPU family (hsx, skx, icx, etc.) matched by the '*'.  Is
			// there some better way of finding this path?
			let sysfs_dir_pat = format!("{}/peci_cpu.{}.*.{}", devdir.display(),
			                            temptype, params.address);

			let typedir = match sysfs::get_single_glob_match(&sysfs_dir_pat) {
				Ok(d) => d,
				Err(e) => {
					error!("No {} subdirectory for PECI device {}: {}",
					       temptype, params.sysfs_name(), e);
					continue;
				},
			};

			let inputs = match sysfs::scan_hwmon_input_files(&typedir, Some("temp")) {
				Ok(v) => v,
				Err(e) => {
					error!("Error finding input files in {}: {}",
					       typedir.display(), e);
					continue;
				},
			};

			for file in inputs {
				if let Err(e) = instantiate_sensor(daemonstate, path,
				                                   file, pecicfg, &physdev).await {
					error!("{}: skipping {} entry: {}", path.0,
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
