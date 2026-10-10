//! A Sigenergy hybrid system: PV, a battery, a house, and the grid in between, behind the
//! Sigenergy register map. An energy manager steers it the way it steers the real one:
//! through Remote EMS (40029/40031) and the charge, discharge, PV and export limits.

use std::time::Duration;

use serde::{Deserialize, Serialize};

use super::bank::Bank;

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Params {
    pub pv_peak_watts: f64,
    pub battery_kwh: f64,
    pub battery_max_watts: f64,
    pub base_load_watts: f64,
    #[serde(default = "half")]
    pub initial_soc: f64,
    #[serde(default = "sunrise")]
    pub sunrise_h: f64,
    #[serde(default = "sunset")]
    pub sunset_h: f64,
}

fn half() -> f64 {
    0.5
}
fn sunrise() -> f64 {
    6.5
}
fn sunset() -> f64 {
    19.5
}

/// Remote EMS control modes (Sigenergy Modbus protocol, appendix 6).
pub fn mode_name(mode: u16) -> &'static str {
    match mode {
        0 => "PCS remote control",
        1 => "standby",
        2 => "maximum self-consumption",
        3 => "command charging, grid first",
        4 => "command charging, PV first",
        5 => "command discharging, PV first",
        6 => "command discharging, ESS first",
        _ => "unknown mode",
    }
}

pub struct Plant {
    params: Params,
    elapsed: Duration,
    soc: f64,
    sky: &'static str,
    pv_wh: f64,
    load_wh: f64,
    import_wh: f64,
    export_wh: f64,
    charge_wh: f64,
    discharge_wh: f64,
}

/// One second of the plant. Load convention for the grid: import positive.
#[derive(Clone, Copy, Debug, Default)]
pub struct Flows {
    pub pv: f64,
    pub load: f64,
    /// Positive: charging.
    pub battery: f64,
    pub grid: f64,
    pub soc: f64,
    pub remote: Option<u16>,
}

impl Plant {
    pub fn new(params: Params) -> Self {
        Self {
            soc: params.initial_soc.clamp(0.0, 1.0),
            params,
            elapsed: Duration::ZERO,
            sky: "auto",
            pv_wh: 0.0,
            load_wh: 0.0,
            import_wh: 0.0,
            export_wh: 0.0,
            charge_wh: 0.0,
            discharge_wh: 0.0,
        }
    }

    pub const MODES: &'static [&'static str] = &["auto", "sunny", "cloudy", "night"];

    pub fn set_sky(&mut self, sky: &str) -> bool {
        match Self::MODES.iter().find(|m| **m == sky) {
            Some(m) => {
                self.sky = m;
                true
            }
            None => false,
        }
    }

    /// Writes the nameplate and the defaults an installer leaves in place.
    pub fn initialise(&self, bank: &mut Bank, serial: &str) {
        let p = &self.params;
        let kw = |w: f64| w / 1000.0;
        bank.set("inverter_model_type", "SigenStor (hemslab)");
        bank.set("inverter_serial_number", serial);
        bank.set("inverter_machine_firmware_version", "hemslab-0.2");
        bank.set("inverter_rated_active_power", kw(p.pv_peak_watts.max(p.battery_max_watts)));
        bank.set("inverter_max_active_power", kw(p.pv_peak_watts.max(p.battery_max_watts)));
        bank.set("inverter_rated_battery_capacity", p.battery_kwh);
        bank.set("inverter_ess_rated_charge_power", kw(p.battery_max_watts));
        bank.set("inverter_ess_rated_discharge_power", kw(p.battery_max_watts));
        bank.set("inverter_ess_battery_soh", 100.0);
        bank.set("plant_max_active_power", kw(p.pv_peak_watts.max(p.battery_max_watts)));
        bank.set("plant_max_apparent_power", kw(p.pv_peak_watts.max(p.battery_max_watts)));
        bank.set("plant_ess_rated_charging_power", kw(p.battery_max_watts));
        bank.set("plant_ess_rated_discharging_power", kw(p.battery_max_watts));
        bank.set("plant_ess_rated_energy_capacity", p.battery_kwh);
        bank.set("plant_ess_charge_cut_off_soc", 100.0);
        bank.set("plant_ess_discharge_cut_off_soc", 5.0);
        bank.set("plant_grid_code_rated_frequency", 50.0);
        bank.set("plant_grid_code_rated_voltage", 230.0);
        bank.set("plant_grid_sensor_status", 1.0);
        // Holding registers: what the plant starts with.
        bank.set("plant_start_stop", 1.0);
        bank.set("plant_remote_ems_enable", 0.0);
        bank.set("plant_remote_ems_control_mode", 2.0);
        bank.set("plant_ess_max_charging_limit", kw(p.battery_max_watts));
        bank.set("plant_ess_max_discharging_limit", kw(p.battery_max_watts));
        bank.set("plant_pv_max_power_limit", kw(p.pv_peak_watts));
        bank.set("plant_grid_point_maximum_export_limitation", 100.0);
        bank.set("plant_grid_maximum_import_limitation", 100.0);
        bank.set("plant_backup_soc", 10.0);
        bank.set("plant_charge_cut_off_soc", 100.0);
        bank.set("plant_discharge_cut_off_soc", 5.0);
    }

    /// Advances one step under the controls currently in the holding registers, and
    /// publishes the result.
    pub fn step(&mut self, dt: Duration, hour_of_day: f64, bank: &mut Bank) -> Flows {
        self.elapsed += dt;
        let t = self.elapsed.as_secs_f64();
        let p = self.params.clone();
        let w = |name: &str| bank.number(name) * 1000.0;

        let running = bank.number("plant_start_stop") != 0.0;
        let remote = (bank.number("plant_remote_ems_enable") != 0.0)
            .then(|| bank.number("plant_remote_ems_control_mode") as u16);
        let max_charge = w("plant_ess_max_charging_limit").min(p.battery_max_watts);
        let max_discharge = w("plant_ess_max_discharging_limit").min(p.battery_max_watts);
        let pv_limit = w("plant_pv_max_power_limit");
        let export_limit = w("plant_grid_point_maximum_export_limitation");
        let import_limit = w("plant_grid_maximum_import_limitation");
        let cut_off = bank.number("plant_discharge_cut_off_soc").max(0.0) / 100.0;
        let charge_cap = bank.number("plant_charge_cut_off_soc").clamp(0.0, 100.0) / 100.0;

        // The sun and the house.
        let day = (hour_of_day - p.sunrise_h) / (p.sunset_h - p.sunrise_h);
        let sun = match self.sky {
            "sunny" | "cloudy" => 1.0,
            "night" => 0.0,
            _ if (0.0..=1.0).contains(&day) => (std::f64::consts::PI * day).sin().max(0.0),
            _ => 0.0,
        };
        let cloud = match self.sky {
            "cloudy" => 0.3 + 0.05 * (t / 7.0).sin(),
            "sunny" => 1.0,
            _ => {
                let wave = 0.5 * (t / 173.0).sin() + 0.3 * (t / 41.0).sin() + 0.2 * (t / 11.0).sin();
                (0.75 + 0.25 * wave).clamp(0.35, 1.0)
            }
        };
        let kettle = if t % 1800.0 < 180.0 { 2000.0 } else { 0.0 };
        let load = p.base_load_watts * (1.0 + 0.15 * (t / 97.0).sin()) + kettle;
        let mut pv = if running { (p.pv_peak_watts * sun * cloud).min(pv_limit) } else { 0.0 };

        // The battery, positive when charging.
        let room = ((charge_cap - self.soc) * p.battery_kwh * 3_600_000.0 / dt.as_secs_f64()).max(0.0);
        let stored = ((self.soc - cut_off) * p.battery_kwh * 3_600_000.0 / dt.as_secs_f64()).max(0.0);
        let wanted = match (running, remote) {
            (false, _) => 0.0,
            (true, Some(0)) => pv - w("plant_active_power_fixed_target"),
            (true, Some(1)) => 0.0,
            (true, Some(3) | Some(4)) => max_charge,
            (true, Some(5) | Some(6)) => -max_discharge,
            // Maximum self-consumption, remote or local: the surplus goes in, the deficit out.
            (true, _) => pv - load,
        };
        let battery = wanted.clamp(-max_discharge.min(stored), max_charge.min(room));

        let mut grid = load - pv + battery;
        if grid < -export_limit {
            // Too much going out: the plant curtails its PV to the export limit.
            pv = (pv - (-export_limit - grid)).max(0.0);
            grid = load - pv + battery;
        }
        let grid = grid.min(import_limit.max(load));

        let hours = dt.as_secs_f64() / 3600.0;
        self.soc = (self.soc + battery * hours / (p.battery_kwh * 1000.0)).clamp(0.0, 1.0);
        self.pv_wh += pv * hours;
        self.load_wh += load * hours;
        if grid >= 0.0 {
            self.import_wh += grid * hours;
        } else {
            self.export_wh += -grid * hours;
        }
        if battery >= 0.0 {
            self.charge_wh += battery * hours;
        } else {
            self.discharge_wh += -battery * hours;
        }

        let kw = |w: f64| w / 1000.0;
        let kwh = |wh: f64| wh / 1000.0;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or_default();
        bank.set("plant_system_time", now.floor());
        bank.set("plant_running_state", if running { 1.0 } else { 3.0 });
        bank.set("inverter_running_state", if running { 1.0 } else { 3.0 });
        bank.set("plant_ems_work_mode", if remote.is_some() { 7.0 } else { 0.0 });
        bank.set("plant_grid_sensor_active_power", kw(grid));
        for phase in ["a", "b", "c"] {
            bank.set(&format!("plant_grid_sensor_phase_{phase}_active_power"), kw(grid / 3.0));
            bank.set(&format!("plant_phase_{phase}_active_power"), kw((pv - battery) / 3.0));
        }
        bank.set("plant_ess_soc", self.soc * 100.0);
        bank.set("inverter_ess_battery_soc", self.soc * 100.0);
        bank.set("plant_active_power", kw(pv - battery));
        bank.set("inverter_active_power", kw(pv - battery));
        bank.set("plant_sigen_photovoltaic_power", kw(pv));
        bank.set("inverter_pv_power", kw(pv));
        bank.set("plant_ess_power", kw(battery));
        bank.set("inverter_ess_charge_discharge_power", kw(battery));
        bank.set("plant_general_load_power", kw(load));
        bank.set("plant_total_load_power", kw(load));
        bank.set("plant_available_max_active_power", kw(p.pv_peak_watts.max(p.battery_max_watts)));
        bank.set("inverter_ess_max_battery_charge_power", kw(p.battery_max_watts));
        bank.set("inverter_ess_max_battery_discharge_power", kw(p.battery_max_watts));
        bank.set("inverter_ess_available_battery_charge_energy", (1.0 - self.soc) * p.battery_kwh);
        bank.set("inverter_ess_available_battery_discharge_energy", (self.soc - cut_off).max(0.0) * p.battery_kwh);
        bank.set("plant_accumulated_pv_energy", kwh(self.pv_wh));
        bank.set("plant_accumulated_consumed_energy", kwh(self.load_wh));
        bank.set("plant_accumulated_grid_import_energy", kwh(self.import_wh));
        bank.set("plant_accumulated_grid_export_energy", kwh(self.export_wh));
        bank.set("plant_accumulated_battery_charge_energy", kwh(self.charge_wh));
        bank.set("plant_accumulated_battery_discharge_energy", kwh(self.discharge_wh));
        bank.set("inverter_ess_accumulated_charge_energy", kwh(self.charge_wh));
        bank.set("inverter_ess_accumulated_discharge_energy", kwh(self.discharge_wh));

        Flows {
            pv,
            load,
            battery,
            grid,
            soc: self.soc,
            remote,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::map::Map;
    use super::*;

    fn plant() -> (Plant, Bank) {
        let plant = Plant::new(Params {
            pv_peak_watts: 10_000.0,
            battery_kwh: 10.0,
            battery_max_watts: 5_000.0,
            base_load_watts: 500.0,
            initial_soc: 0.5,
            sunrise_h: 6.0,
            sunset_h: 20.0,
        });
        let mut bank = Bank::new(Arc::new(Map::load("sigenergy").unwrap()));
        plant.initialise(&mut bank, "T1");
        (plant, bank)
    }

    #[test]
    fn at_noon_the_surplus_charges_the_battery() {
        let (mut plant, mut bank) = plant();
        plant.set_sky("sunny");
        let f = plant.step(Duration::from_secs(1), 13.0, &mut bank);
        assert!(f.battery > 0.0 && f.battery <= 5_000.0, "{f:?}");
        assert!((bank.number("plant_ess_soc") - 50.0).abs() < 0.2);
        assert_eq!(bank.number("plant_ems_work_mode"), 0.0);
    }

    #[test]
    fn remote_ems_forces_charging_from_the_grid_at_night() {
        let (mut plant, mut bank) = plant();
        plant.set_sky("night");
        bank.set("plant_remote_ems_enable", 1.0);
        bank.set("plant_remote_ems_control_mode", 3.0);
        bank.set("plant_ess_max_charging_limit", 3.0);
        let f = plant.step(Duration::from_secs(1), 2.0, &mut bank);
        assert_eq!(f.battery, 3_000.0);
        assert!(f.grid > 3_000.0, "the grid pays for it: {f:?}");
        assert_eq!(bank.number("plant_ems_work_mode"), 7.0);
        assert_eq!(bank.number("plant_ess_power"), 3.0);
    }

    #[test]
    fn an_export_limit_curtails_pv() {
        let (mut plant, mut bank) = plant();
        plant.set_sky("sunny");
        bank.set("plant_remote_ems_enable", 1.0);
        bank.set("plant_remote_ems_control_mode", 1.0); // standby: battery idle
        bank.set("plant_grid_point_maximum_export_limitation", 2.0);
        let f = plant.step(Duration::from_secs(1), 13.0, &mut bank);
        assert!((f.grid + 2_000.0).abs() < 1.0, "exports exactly the limit: {f:?}");
    }
}
