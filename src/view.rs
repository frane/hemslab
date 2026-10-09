//! What each device shows the outside world: the UI reads it, the device loop writes it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use serde::Serialize;
use tokio::sync::mpsc;

const HISTORY: usize = 600;
const LOG: usize = 80;

/// A request from the UI to a device.
#[derive(Debug)]
pub enum Control {
    /// Switch the behaviour mode (`auto`, `plugged`, `cloudy`, …).
    Mode(String),
}

/// One device as the UI sees it.
#[derive(Debug)]
pub struct Handle {
    pub instance: String,
    pub profile: String,
    pub description: String,
    /// `generic` or `replay`.
    pub kind: &'static str,
    pub ski: String,
    pub port: u16,
    pub view: Mutex<View>,
    pub control: mpsc::UnboundedSender<Control>,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct View {
    /// LPC/LPP state as the spec spells it, for generic devices.
    pub state: Option<String>,
    /// `consumption` (LPC) or `production` (LPP).
    pub direction: Option<&'static str>,
    pub limit_watts: Option<f64>,
    pub failsafe_watts: Option<f64>,
    pub nominal_max_watts: Option<f64>,
    /// Load convention: consumption positive.
    pub watts: Option<f64>,
    pub status: String,
    pub mode: String,
    pub modes: Vec<&'static str>,
    pub peers: Vec<String>,
    /// `[unix seconds, watts, limit]`, one per second.
    pub history: VecDeque<(f64, f64, Option<f64>)>,
    /// `[unix seconds, text]`, newest last.
    pub log: VecDeque<(f64, String)>,
}

impl Handle {
    pub fn update(&self, change: impl FnOnce(&mut View)) {
        if let Ok(mut view) = self.view.lock() {
            change(&mut view);
        }
    }

    /// Adds a line to the device's log, and to the process log.
    pub fn log(&self, text: impl Into<String>) {
        let text = text.into();
        tracing::info!("{}: {text}", self.instance);
        self.update(|view| {
            view.log.push_back((now(), text));
            while view.log.len() > LOG {
                view.log.pop_front();
            }
        });
    }

    pub fn sample(&self, watts: f64, limit: Option<f64>) {
        self.update(|view| {
            view.watts = Some(watts);
            view.history.push_back((now(), watts, limit));
            while view.history.len() > HISTORY {
                view.history.pop_front();
            }
        });
    }
}

pub fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or_default()
}

/// Every running device, for the UI.
#[derive(Default)]
pub struct Registry {
    pub devices: Mutex<Vec<Arc<Handle>>>,
}

impl Registry {
    pub fn add(&self, handle: Arc<Handle>) {
        if let Ok(mut devices) = self.devices.lock() {
            devices.push(handle);
        }
    }

    pub fn get(&self, instance: &str) -> Option<Arc<Handle>> {
        self.devices
            .lock()
            .ok()?
            .iter()
            .find(|d| d.instance == instance)
            .cloned()
    }

    pub fn all(&self) -> Vec<Arc<Handle>> {
        self.devices.lock().map(|d| d.clone()).unwrap_or_default()
    }
}
