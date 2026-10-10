# hemslab

Simulated energy devices for people who build energy managers (HEMS). Heat pumps, PV inverters, wallboxes and replayed real devices that talk **EEBUS** on your network like the real thing: they pair, take power limits, fall back to failsafe and report what they draw.

No hardware, no installer, no waiting for the sun.

[Deutsch](README.de.md) · Built on [hupe1980/eebus](https://github.com/hupe1980/eebus)

> **Status: early.** EEBUS and Modbus TCP. SunSpec and OCPP come next.

## Quick start

Download the binary for your system from the [latest release](https://github.com/frane/hemslab/releases/latest) and run:

```bash
./hemslab run heat-pump pv-inverter wallbox elli-charger-2
```

Then open **http://localhost:8080**.

Each device gets its own SKI and port (4712, 4713, …) and announces itself via mDNS. Your energy manager finds them like real devices. By default they pair with anyone who asks.

![hemslab web UI](docs/ui.png)

The web UI shows each device's power, the limit in force (the red dashed line), its LPC/LPP state and every write it received. From the UI you can:

- set and release limits through a built-in test energy manager, so you can try everything without a HEMS
- switch the device's situation: plug the car in or out, sun, clouds or night, heat pump on or off

```
heat-pump-1                      port 4712   SKI 64a3179da57d49f3d8ff7512b6244e7add6e190c
pv-inverter-1                    port 4713   SKI …
wallbox-1                        port 4714   SKI …
elli-charger-2-1                 port 4715   SKI …

wallbox-1: init — limit 4200 W
wallbox-1: charging (4140 W)
wallbox-1: pairing with ea9d2f86… (dialled at 192.168.1.10:4711)
wallbox-1: write to [1]/1: {"loadControlLimitListData":{"loadControlLimitData":[{"isLimitActive":true,"limitId":1,"value":{"number":3000,"scale":0}}]}}
wallbox-1: limited — limit 3000 W
wallbox-1: paused by limit (0 W)
```

The log shows every write your energy manager sends, in plain JSON, and what the device does about it.

## Devices

`hemslab profiles` lists them.

**Generic devices** implement their use cases by the spec (from the `eebus` crate) and behave like real ones:

| Profile | Use cases | Behaviour |
|---|---|---|
| `heat-pump` | LPC, MPC | 6 kW compressor cycling 20 min on and 10 min off. Modulates down to a limit and stops below 1.5 kW. |
| `pv-inverter` | LPP, MPC | 9.5 kW peak following the sun on local time, with passing clouds. A limit curtails production. |
| `wallbox` | LPC, MPC | 3 × 6–16 A. A car comes for an hour and leaves for 15 minutes. Charges in whole amps and pauses below 6 A, like real wallboxes. |

All three start in the LPC/LPP `init` state with their failsafe limit, fall back to failsafe when the energy manager goes silent, and report power, per-phase power and current, and energy.

**Modbus devices** speak Modbus TCP on their own port (1502, 1503, …):

| Profile | Behaviour |
|---|---|
| `sigenergy` | A Sigenergy hybrid system behind the real register map (355 registers, unit 247 for the plant, 1 for the inverter): 10 kWp PV, a 10 kWh battery, a house with a kettle. Remote EMS (40029/40031) with all control modes, charge/discharge limits, PV limit and export limit work as documented, and every write shows up in the log. |

**Replayed devices** are real devices as recorded: the same SPINE structure, use cases and answers. Recorded measurements play back in a loop.

| Profile | Source |
|---|---|
| `elli-charger-2` | Elli Charger 2, complete with 15 minutes of measurements ([elli-eebus-proxy](https://github.com/frane/elli-eebus-proxy) capture) |
| `elli-charger-connect-pro`, `porsche-mobile-charger-connect`, `spelsberg-wallbox-smart-pro` | Wallboxes, structure and use cases |
| `kostal-smart-energy-meter`, `sma-home-manager-2` | Energy meters and managers |
| `vaillant-arotherm`, `viessmann-vitoconnect` | Heat pumps |

The Sigenergy register map comes from [Sigenergy-Local-Modbus](https://github.com/TypQxQ/Sigenergy-Local-Modbus) (MIT), which follows Sigenergy's Modbus protocol V2.9. The structural captures come from [enbility/devices](https://github.com/enbility/devices) (MIT). See [`profiles/real/README.md`](profiles/real/README.md).

Use `hemslab show <profile>` to see entities, features, functions and use cases.

## Your own devices

**Find** the device and its SKI:

```bash
hemslab discover
# 3d9f…a0c1  LG               THERMA V                192.168.1.42:4712
```

**Record** it. hemslab connects as an energy manager, reads everything the device serves and subscribes to every feature:

```bash
hemslab record 3d9f…a0c1 --name lg-therma-v --minutes 15
```

Pair **hemslab-recorder** in the device's EEBUS settings when it shows up. Some devices accept only one energy manager; remove yours there for the recording. When the time is up, or on Ctrl-C, you get `lg-therma-v.jsonl`, the raw capture, and `lg-therma-v.json`, the profile. Serial numbers are replaced with `X`.

**Modbus devices** are recorded through their register map: every register is read at an interval and the profile replays the values as they moved. Serial numbers are replaced with `X`.

```bash
hemslab record-modbus 192.168.1.30 --map sigenergy --name my-sigenergy --minutes 15
```

Sigenergy needs "Modbus TCP Server" enabled (often by the installer). Maps for other devices are JSON files of the same shape as [`profiles/maps/sigenergy.json`](profiles/maps/sigenergy.json); pass the file to `--map`.

Captures from [elli-eebus-proxy](https://github.com/frane/elli-eebus-proxy), whose `traffic.jsonl` includes what an energy manager wrote, and single discovery or use-case datagrams import too: `hemslab import traffic.jsonl --name …` (`--no-redact` keeps serial numbers).

**Run** it with `hemslab run lg-therma-v.json`. Pull requests with new profiles are welcome.

**Write** a generic profile in TOML. Copy one from [`profiles/`](profiles/) and change the device type, limits or behaviour.

## Options

| Option | Environment | Default | |
|---|---|---|---|
| `--state-dir` | `HEMSLAB_STATE` | `state` (`/data` in Docker) | Identities and trust, one directory per device. Keep it, or energy managers have to pair again. |
| `--port` | `HEMSLAB_PORT` | 4712 | Port of the first device; the others count up |
| `--dial` | `HEMSLAB_DIAL` | – | `host:port` or `SKI@host:port` of an energy manager to dial, for one that does not browse mDNS |
| `--trust` | `HEMSLAB_TRUST` | – | SKIs to trust in advance |
| `--strict` | `HEMSLAB_STRICT` | off | Pair only with `--trust` |
| `--no-mdns` | `HEMSLAB_NO_MDNS` | off | Don't announce |
| `--announce-ip` | `HEMSLAB_ANNOUNCE_IP` | detected | Address to announce on hosts with several networks |
| profiles | `HEMSLAB_DEVICES` | – | Comma-separated list, for Docker Compose |

| `--modbus-port` | `HEMSLAB_MODBUS_PORT` | 1502 | Port of the first Modbus TCP device; the others count up |
| `--ui` | `HEMSLAB_UI` | `0.0.0.0:8080` | Address of the web UI |
| `--no-ui` | `HEMSLAB_NO_UI` | off | No web UI and no test energy manager |

From source: `cargo install --git https://github.com/frane/hemslab`. For a NAS there is a Docker image (`docker-compose.yml`; host networking for mDNS).

## Tested against

- **The `eebus` crate's Energy Guard and Monitoring Appliance:** LPC limit, release and MPC readings, for generic and replayed devices, plus the web UI (`cargo test`).
- **eebus-go, the stack behind evcc, as an Energy Guard:** 3000 W, 7000 W and release reach the wallbox. It pauses at 3 kW and charges at 10 A at 7 kW. MPC power arrives every second.
- **[elli-eebus](https://github.com/frane/elli-eebus), the library behind the Home Assistant integration [ha-elli](https://github.com/frane/ha-elli):** it reads the replayed `elli-charger-2` exactly like the real wallbox (brand, 6–16 A, failsafe) and sets limits and failsafe on it.
- **The full chain without hardware:** eebus-go as energy manager → [elli-eebus-proxy](https://github.com/frane/elli-eebus-proxy) → replayed Elli. Limits 3000 W, 0 W and release come through.

## Roadmap

- Recording between a device and its energy manager (MITM), to capture what the energy manager writes
- Quirks per profile, e.g. the Elli Charger 2 ignoring `timePeriod` or rejecting pause without OSCEV
- E-mobility use cases (EVSECC, EVCC, OPEV, OSCEV) for the wallbox, and batteries
- More Modbus devices and maps (SunSpec inverters, heat pumps such as the LG Therma V), then OCPP

## License

MIT
