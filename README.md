# Mercury IoT (Rust edition)

A BME280 sensor node written in Rust, feeding a Rust Azure IoT Edge module, feeding Azure IoT Hub.

```
BME280 --I2C--> Pico H (Rust firmware) --USB serial (JSON lines)--> Pi 5
Pi 5: IoT Edge runtime -> mercuryRust module -> edgeHub --$upstream--> Azure IoT Hub
```

Two codebases make this up. This README covers both so the whole build can be repeated end to end.

|Part|Repo|Runs on|Language|
|-|-|-|-|
|Pico firmware (`mercury-pico`)|*link to Pico repo*|Raspberry Pi Pico H (RP2040)|Rust, `no\_std`, Embassy|
|Edge module (`mercury-rust`)|*link to Pi repo*|Raspberry Pi 5, in a Docker container under IoT Edge|Rust, Tokio|

Values used in the original build (swap in your own):

|Thing|Value|
|-|-|
|IoT Hub (F1 free tier)|`mercury-iot-hub`|
|Edge device identity|`edge-device-01`|
|Module name|`mercuryRust`|
|Container registry|`hendersonconsultregistry.azurecr.io`|
|Pi hostname / user|`piiot.local` / `mhenderson`|
|Pico serial device on the Pi|`/dev/ttyACM0`|

## Hardware

* Raspberry Pi 5 (8 GB), microSD, official power supply
* Raspberry Pi Pico H (RP2040, pre-soldered headers, non-wireless)
* BME280 breakout. **Power it from 3.3 V only; 5 V will damage it.**
* Breadboard and jumper wires

BME280 wiring (I2C0):

|BME280|Pico|
|-|-|
|VIN|3V3 OUT (pin 36)|
|GND|GND (pin 38)|
|SCL|GP1 (pin 2)|
|SDA|GP0 (pin 1)|

I2C address is 0x76 (0x77 if the SDO pin is tied high).

## Development environment

* Windows 11 laptop, VS Code, WSL 2 (Ubuntu). Work in the Linux filesystem (`\~/software-projects`), not `/mnt/c`.
* Extensions: rust-analyzer, CodeLLDB, Even Better TOML, Dependi, plus WSL and Remote - SSH. Extensions install per environment, so install them again inside WSL and on the Pi.
* Use **two VS Code windows**: one connected to WSL (Pico firmware), one connected to the Pi over Remote-SSH (edge module). Open VS Code normally, not as Administrator.
* Rust everywhere via rustup.rs.

\---

## Part 1: Pico firmware (`mercury-pico`)

### One-time toolchain setup (WSL)

```
sudo apt update \&\& sudo apt install -y build-essential pkg-config libudev-dev
rustup target add thumbv6m-none-eabi
cargo install elf2uf2-rs --locked
```

`elf2uf2-rs` has no `--version` flag; `which elf2uf2-rs` confirms it installed.

### Project layout

```
mercury-pico/
  .cargo/config.toml   # \[build] target = "thumbv6m-none-eabi"
  build.rs             # copies memory.x, adds linker args --nmagic, -Tlink.x, -Tlink-rp.x
  memory.x             # RP2040 flash/RAM regions (BOOT2 256 bytes + 2 MB flash, 264 KB RAM)
  Cargo.toml
  src/main.rs
```

Dependencies (edition 2024):

* `embassy-executor` 0.10 (features `platform-cortex-m`, `executor-thread`, `executor-interrupt`)
* `embassy-rp` 0.10 (features `unstable-pac`, `time-driver`, `critical-section-impl`, `rp2040`)
* `embassy-time` 0.5, `embassy-usb` 0.6, `static\_cell` 2
* `portable-atomic` with feature `critical-section` (the RP2040 has no hardware atomic compare-and-swap; `static\_cell` fails to build without this)
* `embedded-hal-async` 1, `cortex-m` 0.7 (`inline-asm`), `cortex-m-rt` 0.7, `panic-halt`

The Embassy example project at the matching release tag (`embassy-rp-v0.10.0`, `examples/rp`) is the reference for `build.rs`, `memory.x`, and the feature flags.

### What the firmware does

* Presents a USB CDC serial port (shows up as `/dev/ttyACM0` on the Pi, a COM port on Windows).
* Every 5 seconds reads the BME280 and writes one JSON line:

```json
  {"node":"pico1\_bme280","sensor\_id":"<fixed GUID>","temperature\_c":22.6,"pressure\_hpa":995.9,"humidity\_pct":50.5,"timestamp\_ms":87545}
  ```

* On a sensor failure it prints `{"node":"pico1\_bme280","error":"bme280 read failed"}` and retries initialising the sensor on later passes.
* `sensor\_id` is a hardcoded GUID, generated once, so readings stay traceable to a node across reflashes and sensor swaps.
* The BME280 driver is hand-written (ported from the original MicroPython code) using the same registers and compensation formulas, and `embedded-hal-async` for I2C. The `bme280` crate's async mode was tried first, but it needs a nightly compiler.
* USB writes are chunked to 64 bytes and time out after 500 ms, so a missing reader never blocks the loop. The first line after opening the port may be a truncated fragment; the Pi side ignores non-JSON lines.

### Build, flash, verify

```
cargo build --release
elf2uf2-rs target/thumbv6m-none-eabi/release/mercury-pico mercury-pico.uf2
```

1. Hold **BOOTSEL** while plugging the Pico into the laptop. An `RPI-RP2` drive appears.
2. In File Explorer, press **F5** (WSL folders don't refresh by themselves), then drag `mercury-pico.uf2` from `\\\\wsl.localhost\\Ubuntu\\home\\<user>\\software-projects\\mercury-pico` onto `RPI-RP2`. The drive disappears as the Pico reboots.
3. Read the output from a regular Windows PowerShell (WSL can't see the COM port):

```
   \[System.IO.Ports.SerialPort]::GetPortNames()
   $p = New-Object System.IO.Ports.SerialPort COM4,115200   # use the port listed above
   $p.Open()
   while ($true) { $p.ReadLine() }
   ```

   Ctrl+C to stop, then `$p.Close()` before reflashing.

The warning about `proc-macro-error2` during builds comes from a dependency and is harmless.

### Rolling back to MicroPython

Back up the old `main.py` (and note the `sensor\_id`) before the first flash. To go back: hold BOOTSEL, drag the MicroPython `.uf2` (from micropython.org's Pico page) onto `RPI-RP2`, then copy `main.py` onto the Pico.

\---

## Part 2: Pi 5 edge module (`mercury-rust`)

### Pi prerequisites (done once)

* Raspberry Pi OS **Bookworm (Legacy, 64-bit) Lite**. Trixie has unresolved dependency conflicts with the IoT Edge identity service.
* IoT Edge runtime (aziot-edge 1.6 LTS), provisioned against the hub with symmetric-key auth. The `edgeAgent` and `edgeHub` containers should be running (`sudo iotedge list`).
* Docker (Moby) engine.

### Dev setup on the Pi (Remote-SSH)

```
sudo apt install -y build-essential pkg-config libssl-dev curl git
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source \~/.cargo/env
sudo usermod -aG dialout $USER     # then reconnect, so serial permission applies
```

Run the commands one at a time; chaining the `apt` and `curl` lines onto one line breaks.

### Crates

`tokio` (features `rt-multi-thread`, `macros`, `io-util`, `time`, `net`, `signal`), `tokio-serial`, `serde\_json`, `rumqttc`, `base64`, `urlencoding`.

### What the module does

* **Serial reader task** per Pico: reads JSON lines, skips anything that isn't valid JSON, and overwrites the latest reading for that node in a shared map (sample-and-hold). The Picos run at 5 s; cloud egress is decoupled from that.
* **Send loop** every **30 seconds** (about 2,880 messages/day, inside the F1 limit of 8,000/day, which blocks the whole hub when exceeded). It merges the latest reading from each node into one JSON object keyed by node name and adds `age\_sec` to each so stale data is visible.
* **Authentication via the Edge workload API** (no connection string anywhere): the module reads its `IOTEDGE\_\*` environment variables, talks HTTP to the Unix socket in `IOTEDGE\_WORKLOADURI`, asks it to sign a SAS token (`POST /modules/{id}/genid/{genid}/sign`), and fetches the CA (`GET /trust-bundle`).
* **Transport:** MQTT over TLS to `edgeHub` at `IOTEDGE\_GATEWAYHOSTNAME:8883`.

  * Client id `<deviceId>/<moduleId>`
  * Username `<hubHostname>/<deviceId>/<moduleId>/?api-version=2018-06-30`, password = the signed SAS token
  * Publish topic `devices/<deviceId>/modules/<moduleId>/messages/events/$.ct=application%2Fjson\&$.ce=utf-8` (the `$.ct`/`$.ce` properties make IoT Hub treat the body as JSON, not an escaped string)
* **Token refresh:** a supervisor task rebuilds the MQTT connection with a fresh token every 50 minutes (tokens last 1 hour), and after any connection error. A reading that lands while disconnected is skipped, not buffered.
* **Signal handling:** the process runs as PID 1 in the container, so it handles SIGTERM/SIGINT itself; without that, `docker stop` hangs for 10 seconds and Ctrl+C needs three presses.

### Container

Multi-stage `Dockerfile`: build stage `rust:1-bookworm` (adds `pkg-config libudev-dev`), runtime stage `debian:bookworm-slim` (adds `ca-certificates libudev1`). `.dockerignore` excludes `target` and `.git`.

```
sudo docker build -t <registry>/mercury-rust:<tag> .
sudo docker login <registry>        # registry admin credentials, typed at the prompt
sudo docker push <registry>/mercury-rust:<tag>
```

**Use a new tag for every deployed change.** Edge won't re-pull a tag it already has. The "legacy builder is deprecated" warning is harmless; `sudo apt install moby-buildx` silences it.

### Deploying (Azure portal: IoT Hub > Devices > edge-device-01 > Set modules)

* Add the module: name `mercuryRust`, image URI `<registry>/mercury-rust:<tag>`.
* Container create options (passes the Pico through; Docker needs the path to exist when the container starts):

```json
  {"HostConfig":{"Devices":\[{"PathOnHost":"/dev/ttyACM0","PathInContainer":"/dev/ttyACM0","CgroupPermissions":"rwm"}]}}
  ```

* Route: `FROM /messages/modules/mercuryRust/\* INTO $upstream`
* Registry credentials go in the container registry section of the same page.
* To pause a module without deleting it, set its **Desired Status** to `stopped` in the deployment. A plain `docker stop` only lasts a few minutes because Edge Agent restarts it.

### Verifying

```
sudo iotedge list                          # mercuryRust running, correct image tag
sudo iotedge logs mercuryRust --tail 20    # "connected to edgeHub", then "published: {...}" every 30 s
az iot hub monitor-events -n mercury-iot-hub -d edge-device-01
```

Check after about an hour that the refresh happened:

```
sudo iotedge logs mercuryRust | grep -E "refreshing|connected"
```

\---

## Order of operations for a fresh build

1. Wire the BME280 to the Pico; set up the WSL toolchain.
2. Build and flash the Pico firmware; confirm JSON lines in PowerShell.
3. Move the Pico to the Pi's USB port; confirm `ls /dev/ttyACM\*`.
4. Set up Rust, Docker, and serial permission on the Pi.
5. Build, tag, and push the module image; add the module, create options, and route in the portal.
6. Verify with `iotedge list`, logs, and `monitor-events`.

## Gotchas found along the way

|Symptom|Cause / fix|
|-|-|
|`Device or resource busy` on the serial port|Another process holds it: an old container still running (`docker ps`, then `docker kill`), or Edge Agent restarted another module that opens the same port. `sudo fuser -v /dev/ttyACM0` shows the owner.|
|Module can't read the Pico after a replug or reflash|The container's device mapping went stale. `sudo iotedge restart mercuryRust` (or reboot with the Pico already plugged in).|
|Redeployed but the log looks the same|Same image tag (Edge won't re-pull), or the file wasn't saved before the build. Bump the tag; save first; run `cargo check` before the slow Docker build.|
|Firmware build fails on `compare\_exchange`|Add `portable-atomic` with feature `critical-section`.|
|`bme280` crate fails with "`#!\[feature]` may not be used on the stable release channel"|The crate's async mode needs nightly. Use the hand-written driver.|
|`hmac` error about `new\_from\_slice` (older code)|Import the `KeyInit` trait.|
|Old `.uf2` flashed|Explorer shows a stale listing. Press F5 before copying.|
|`az` errors about `\~/.azure/cliextensions` access|Extension files were created by an elevated shell. Reinstall the extension from a normal shell.|
|Messages arrive as an escaped string in `monitor-events`|Missing `$.ct=application%2Fjson\&$.ce=utf-8` on the publish topic.|
|`mqtt: Connection refused` right after a module restart|`edgeHub` isn't listening yet. The supervisor retries after 5 seconds.|

## Not done yet

* Pico #2 with a ToF distance sensor (VL53L0X/L1X, I2C 0x29): new firmware, then add `/dev/ttyACM1` to the module's create options and spawn a second reader task.
* Typed reading structs in the module instead of generic JSON.
* Buffering readings while `edgeHub` is disconnected during a token refresh.
* Reconnect logic for the serial reader if a Pico re-enumerates.

