use std::{
	collections::HashMap,
	path::Path,
	sync::{Arc, Mutex as SyncMutex},
};

use dbus::nonblock::SyncConnection;

use crate::{
	powerstate::PowerState,
	sensor,
	sensor::{
		Sensor,
		SensorConfig,
		SensorConfigMap,
		SensorIntfData,
		SensorIOCtx,
		SensorMap,
	},
	threshold,
	sysfs,
	types::*,
};

#[derive(Debug)]
pub struct PECISensorConfig {
	name: String,
	cpuid: u64,
	bus: u64,
	address: u64,
	dts_crit_offset: f64,
	thresholds: Vec<threshold::ThresholdConfig>,

	// PresenceGPIO in a few E-M configs, seems to be ignored by
	// dbus-sensors though:
	//   [ { Name: "...", Polarity: "..." } ]
	// (why the array i dunno)
}

const PECI_BUS_DIR: &str = "/sys/bus/peci";

// The kernel PECI subsustem's sysfs interface doesn't expose an
// interface to instantiate a device at a given address; it simply
// allows triggering a global rescan operation, so this isn't tied to
// any particular device.
fn rescan() -> ErrResult<()> {
	Ok(std::fs::write(Path::new(PECI_BUS_DIR).join("rescan"), "1")?)
}

impl PECISensorConfig {
	pub fn from_dbus(basecfg: &dbus::arg::PropMap, baseintf: &str, intfs: &HashMap<String, dbus::arg::PropMap>) -> Option<Self> {
		use dbus::arg::prop_cast;
		let name: &String = prop_cast(basecfg, "Name")?;
		let cpuid: u64 = *prop_cast(basecfg, "CpuID")?;
		let bus: u64 = *prop_cast(basecfg, "Bus")?;
		let address: u64 = *prop_cast(basecfg, "Address")?;
		let dts_crit_offset = prop_cast::<f64>(basecfg, "DtsCritOffset").map(|p| *p).unwrap_or(0.0);

		let thresholds = threshold::get_configs_from_dbus(baseintf, intfs);

		if !dts_crit_offset.is_finite() {
			eprintln!("{}: DtsCritOffset must be finite (got {})", name, dts_crit_offset);
			return None;
		}

		Some(Self {
			name: name.clone(),
			cpuid,
			bus,
			address,
			dts_crit_offset,
			thresholds,
		})
	}
}

pub async fn update_sensors(cfgmap: &SensorConfigMap, sensors: &mut SensorMap,
			    dbuspaths: &FilterSet<InventoryPath>, cr: &SyncMutex<dbus_crossroads::Crossroads>,
			    conn: &Arc<SyncConnection>, sensor_intfs: &SensorIntfData) -> ErrResult<()> {
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

			let Some(entry) = sensor::get_nonactive_sensor_entry(sensors, name.clone()).await else {
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

			sensor::install_or_activate(entry, cr, io, sensor_intfs, || {
				Sensor::new(&name, file.kind, sensor_intfs, conn)
					.with_power_state(PowerState::On) // FIXME: make configurable?
					.with_thresholds_from(&pecicfg.thresholds, &sensor_intfs.thresholds, conn)
					.with_minval(-128.0)
					.with_maxval(127.0)
			}).await;
		}
	}

	Ok(())
}
