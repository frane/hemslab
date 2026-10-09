use std::net::IpAddr;
use std::path::{Path, PathBuf};

use clap::{Parser, Subcommand};
use eebus::ship::Ski;
use hemslab::capture::{self, ImportOptions};
use hemslab::device::{self, Spec};
use hemslab::profile::{BUILTIN, Profile};
use hemslab::replay::Structure;
use tokio::sync::{oneshot, watch};

/// Simulated energy devices for HEMS developers.
#[derive(Parser)]
#[command(version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run devices: built-in profile names or profile files. Repeat a name for more.
    Run {
        #[arg(required = true, env = "HEMSLAB_DEVICES", value_delimiter = ',')]
        profiles: Vec<String>,
        /// Where identities and trust are kept, one directory per device.
        #[arg(long, env = "HEMSLAB_STATE", default_value = "state")]
        state_dir: PathBuf,
        /// SHIP port of the first device; the others count up.
        #[arg(long, env = "HEMSLAB_PORT", default_value_t = 4712)]
        port: u16,
        /// Do not announce via mDNS.
        #[arg(long, env = "HEMSLAB_NO_MDNS", value_parser = clap::builder::BoolishValueParser::new())]
        no_mdns: bool,
        /// Address to announce, if the host has several.
        #[arg(long, env = "HEMSLAB_ANNOUNCE_IP")]
        announce_ip: Option<IpAddr>,
        /// Pair only with SKIs given by --trust (default: pair with anyone who asks).
        #[arg(long, env = "HEMSLAB_STRICT", value_parser = clap::builder::BoolishValueParser::new())]
        strict: bool,
        /// SKI of an energy manager to trust in advance.
        #[arg(long, env = "HEMSLAB_TRUST", value_delimiter = ',')]
        trust: Vec<String>,
        /// Address of the web UI.
        #[arg(long, env = "HEMSLAB_UI", default_value = "0.0.0.0:8080")]
        ui: std::net::SocketAddr,
        /// No web UI and no test energy manager.
        #[arg(long, env = "HEMSLAB_NO_UI", value_parser = clap::builder::BoolishValueParser::new())]
        no_ui: bool,
        /// Dial an energy manager instead of waiting for it: `host:port` or
        /// `SKI@host:port` (all devices).
        #[arg(long, env = "HEMSLAB_DIAL", value_delimiter = ',')]
        dial: Vec<String>,
    },
    /// List the built-in profiles.
    Profiles,
    /// Show what a profile contains.
    Show { profile: String },
    /// Turn a capture into a replay profile.
    Import {
        /// `traffic.jsonl` from elli-eebus-proxy, or discovery/use-case datagrams.
        #[arg(required = true)]
        captures: Vec<PathBuf>,
        /// Profile name, e.g. `lg-therma-v`.
        #[arg(long)]
        name: String,
        #[arg(long)]
        description: Option<String>,
        /// Which side of the proxy is the device (default: detected).
        #[arg(long)]
        side: Option<String>,
        /// Keep serial numbers.
        #[arg(long)]
        no_redact: bool,
        /// Seconds of recorded notifications to keep for playback.
        #[arg(long, default_value_t = 900.0)]
        max_timeline: f64,
        /// Output file (default: `<name>.json`).
        #[arg(short, long)]
        output: Option<PathBuf>,
    },
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "hemslab=info".into()),
        )
        .with_target(false)
        .with_ansi(std::io::IsTerminal::is_terminal(&std::io::stderr()))
        .with_writer(std::io::stderr)
        .init();
    let result = match Cli::parse().command {
        Command::Run {
            profiles,
            state_dir,
            port,
            no_mdns,
            announce_ip,
            strict,
            trust,
            dial,
            ui,
            no_ui,
        } => {
            run(profiles, state_dir, port, !no_mdns, announce_ip, strict, trust, dial, (!no_ui).then_some(ui))
                .await
        }
        Command::Profiles => {
            list();
            Ok(())
        }
        Command::Show { profile } => show(&profile),
        Command::Import {
            captures,
            name,
            description,
            side,
            no_redact,
            max_timeline,
            output,
        } => import(&captures, name, description, side, !no_redact, max_timeline, output),
    };
    if let Err(error) = result {
        eprintln!("error: {error}");
        std::process::exit(1);
    }
}

#[allow(clippy::too_many_arguments)]
async fn run(
    names: Vec<String>,
    state_dir: PathBuf,
    port: u16,
    mdns: bool,
    announce_ip: Option<IpAddr>,
    strict: bool,
    trust: Vec<String>,
    dial: Vec<String>,
    ui: Option<std::net::SocketAddr>,
) -> Result<(), String> {
    let dial: Vec<device::Dial> = dial
        .iter()
        .map(|a| a.parse().map_err(|e| format!("--dial {a}: {e}")))
        .collect::<Result<_, _>>()?;
    let mut trust: Vec<Ski> = trust
        .iter()
        .map(|s| s.parse().map_err(|e| format!("--trust {s}: {e}")))
        .collect::<Result<_, _>>()?;
    let (stop_tx, stop_rx) = watch::channel(false);
    let registry = std::sync::Arc::new(hemslab::view::Registry::default());
    let cem = match ui {
        Some(_) => {
            let cem = hemslab::cem::start(registry.clone(), stop_rx.clone())?;
            // The test energy manager is always welcome, even with --strict.
            if let Ok(status) = cem.status.lock()
                && let Ok(ski) = status.ski.parse()
            {
                trust.push(ski);
            }
            Some(cem)
        }
        None => None,
    };
    let mut tasks = Vec::new();
    let mut counts = std::collections::HashMap::<String, usize>::new();
    for (index, spec) in names.iter().enumerate() {
        let profile = Profile::load(spec)?;
        let count = counts.entry(profile.name().to_string()).or_default();
        *count += 1;
        let instance = format!("{}-{}", profile.name(), count);
        let (tx, rx) = oneshot::channel();
        let spec = Spec {
            profile,
            instance,
            port: port + index as u16,
            state_dir: state_dir.clone(),
            mdns,
            announce_ip,
            accept_all: !strict,
            trust: trust.clone(),
            dial: dial.clone(),
        };
        tasks.push(tokio::spawn(device::run(spec, tx, stop_rx.clone())));
        let started = rx.await.map_err(|_| "a device failed to start")??;
        registry.add(started.handle.clone());
        println!(
            "{:<32} port {:<5}  SKI {}",
            started.instance, started.port, started.ski
        );
    }
    if let (Some(address), Some(cem)) = (ui, cem) {
        let bound = hemslab::ui::serve(address, registry.clone(), cem).await?;
        let shown = if bound.ip().is_unspecified() {
            format!("http://localhost:{}", bound.port())
        } else {
            format!("http://{bound}")
        };
        println!("\nWeb UI: {shown}");
    }
    println!(
        "\n{} device(s) running. {}Ctrl-C stops.\n",
        tasks.len(),
        if strict { "" } else { "Pairing with anyone who asks. " }
    );
    tokio::signal::ctrl_c().await.map_err(|e| e.to_string())?;
    let _ = stop_tx.send(true);
    for task in tasks {
        let _ = task.await;
    }
    Ok(())
}

fn list() {
    for (name, _) in BUILTIN {
        let profile = hemslab::profile::builtin(name).expect("built-in");
        let kind = match profile {
            Profile::Generic(_) => "generic",
            Profile::Replay(_) => "replay ",
        };
        println!("{name:<32} {kind}  {}", profile.description());
    }
}

fn show(spec: &str) -> Result<(), String> {
    let profile = Profile::load(spec)?;
    println!("{}: {}", profile.name(), profile.description());
    match profile {
        Profile::Generic(p) => {
            println!("  device   {} {} ({} / {})", p.device.brand, p.device.model, p.device.device_type, p.device.entity_type);
            println!("  limit    {:?}, failsafe {} W, nominal max {} W", p.limit.use_case, p.limit.failsafe_watts, p.limit.nominal_max_watts);
            println!("  mpc      {}", p.mpc);
            println!("  model    {:?}", p.model);
        }
        Profile::Replay(p) => {
            let structure = Structure::of(&p)?;
            println!("  source   {}", p.source);
            println!("  address  {}", p.device_address);
            for entity in &structure.remote.entities {
                let kind = entity.entity_type.as_ref().map(|t| t.as_str()).unwrap_or("?");
                println!("  entity {:?} {kind}", entity.address);
                for feature in &entity.features {
                    println!(
                        "    {:>3} {} ({:?}): {}",
                        feature.address.feature.map(|f| f.get()).unwrap_or(0),
                        feature.feature_type.as_str(),
                        feature.role,
                        feature
                            .functions
                            .iter()
                            .filter_map(|f| f.function.as_ref().map(|f| f.as_str().to_string()))
                            .collect::<Vec<_>>()
                            .join(", ")
                    );
                }
            }
            for use_case in &structure.remote.use_cases {
                println!(
                    "  use case {} as {} {:?}",
                    use_case.name.as_str(),
                    use_case.actor.as_str(),
                    use_case.scenarios
                );
            }
            println!(
                "  data     {} answers, {} notifications over {:.0} s",
                p.data.len(),
                p.timeline.len(),
                p.timeline.last().map(|t| t.at).unwrap_or(0.0)
            );
        }
    }
    Ok(())
}

fn import(
    captures: &[PathBuf],
    name: String,
    description: Option<String>,
    side: Option<String>,
    redact: bool,
    max_timeline: f64,
    output: Option<PathBuf>,
) -> Result<(), String> {
    let paths: Vec<&Path> = captures.iter().map(PathBuf::as_path).collect();
    let options = ImportOptions {
        name: name.clone(),
        description,
        side,
        redact,
        max_timeline_s: max_timeline,
    };
    let (profile, report) = capture::import(&paths, &options)?;
    let output = output.unwrap_or_else(|| PathBuf::from(format!("{name}.json")));
    let json = serde_json::to_string_pretty(&profile).map_err(|e| e.to_string())?;
    std::fs::write(&output, json + "\n").map_err(|e| format!("{}: {e}", output.display()))?;
    println!(
        "{}: {} entities, {} features, {} use cases, {} answers, {} notifications over {:.0} s",
        output.display(),
        report.entities,
        report.features,
        report.use_cases,
        report.data,
        report.timeline,
        report.timeline_s
    );
    if !report.redacted.is_empty() {
        println!("redacted: {}", report.redacted.join(", "));
    }
    Ok(())
}
