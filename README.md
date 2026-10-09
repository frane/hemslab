# hemslab

Simulated energy devices for people who build energy managers (HEMS). Heat pumps, PV inverters, wallboxes and replayed real devices that talk **EEBUS** on your network like the real thing: they pair, take power limits, fall back to failsafe and report what they draw.

No hardware, no installer, no waiting for the sun.

[Deutsch](README.de.md) · Built on [hupe1980/eebus](https://github.com/hupe1980/eebus)

> **Status: 0.1.0, early.** EEBUS only. Modbus/SunSpec and OCPP come next.

## Quick start

```bash
docker run --rm --network host -v ./data:/data fbandov/hemslab run heat-pump pv-inverter wallbox elli-charger-2
```

Each device gets its own SKI and port (4712, 4713, …) and announces itself via mDNS. Your energy manager finds them like real devices. By default they pair with anyone who asks.

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

**Replayed devices** are real devices as recorded: the same SPINE structure, use cases and answers. Recorded measurements play back in a loop.

| Profile | Source |
|---|---|
| `elli-charger-2` | Elli Charger 2, complete with 15 minutes of measurements ([elli-eebus-proxy](https://github.com/frane/elli-eebus-proxy) capture) |
| `elli-charger-connect-pro`, `porsche-mobile-charger-connect`, `spelsberg-wallbox-smart-pro` | Wallboxes, structure and use cases |
| `kostal-smart-energy-meter`, `sma-home-manager-2` | Energy meters and managers |
| `vaillant-arotherm`, `viessmann-vitoconnect` | Heat pumps |

The structural captures come from [enbility/devices](https://github.com/enbility/devices) (MIT). See [`profiles/real/README.md`](profiles/real/README.md).

Use `hemslab show <profile>` to see entities, features, functions and use cases.

## Your own devices

**Record** the device with a proxy between it and an energy manager. For now that's [elli-eebus-proxy](https://github.com/frane/elli-eebus-proxy) in sniff mode, which writes `traffic.jsonl`. A built-in `hemslab record` for any EEBUS device comes next.

**Import** the capture:

```bash
hemslab import traffic.jsonl --name lg-therma-v
# lg-therma-v.json: 2 entities, 12 features, 4 use cases, 18 answers, 600 notifications over 900 s
# redacted: 00015087
```

Serial numbers are replaced with `X` (`--no-redact` keeps them). Single datagrams (discovery or use-case replies, plain or EEBUS JSON) work too.

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

A [`docker-compose.yml`](docker-compose.yml) is included. Docker needs host networking for mDNS.

Without Docker: `cargo install --git https://github.com/frane/hemslab`.

## Tested against

- The `eebus` crate's Energy Guard and Monitoring Appliance: LPC limit, release and MPC readings, for generic and replayed devices (`cargo test`)
- eebus-go (the stack behind evcc) as an Energy Guard: limits 3000 W, 7000 W and release reach the wallbox, which pauses at 3 kW and charges at 10 A at 7 kW. MPC power arrives every second.

## Roadmap

- `hemslab record`: a built-in MITM recorder for any EEBUS device
- Quirks per profile, e.g. the Elli Charger 2 ignoring `timePeriod` or rejecting pause without OSCEV
- E-mobility use cases (EVSECC, EVCC, OPEV, OSCEV) for the wallbox, and batteries
- Modbus/SunSpec, then OCPP
- A web page to watch and control devices (plug a car in, pass a cloud)

## License

MIT
