# Real device structures

These profiles replay the discovery and use-case data of real EEBUS devices. They were
recorded with [eebus-go](https://github.com/enbility/eebus-go) and published as
[enbility/devices](https://github.com/enbility/devices) under the MIT license. The
[eebus](https://github.com/hupe1980/eebus) crate converted them into the EEBUS JSON
projection (`tests/fixtures/devices`), and `hemslab import` turned that into profiles.

They carry structure only, without recorded values. An energy manager sees the same
entities, features, functions and use cases as on the real device. Serial numbers were
redacted upstream, and digit runs in device addresses are redacted again on import.

`../elli-charger-2.json` is a complete recording, values included. It was made with
elli-eebus-proxy on an Elli Charger 2 in October 2026, and its serial number is redacted.
