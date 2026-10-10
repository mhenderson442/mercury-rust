use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use base64::{engine::general_purpose::STANDARD, Engine};
use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS, TlsConfiguration, Transport};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;
use tokio::signal::unix::{signal, SignalKind};
use tokio_serial::SerialPortBuilderExt;

type Latest = Arc<Mutex<HashMap<String, (Value, Instant)>>>;
type Shared = Arc<Mutex<Option<AsyncClient>>>;
type BoxErr = Box<dyn std::error::Error + Send + Sync>;

const SERIAL_PORT: &str = "/dev/ttyACM0";
const SERIAL_BAUD: u32 = 115_200;
const SEND_INTERVAL_SECS: u64 = 30; // F1 tier allows 8,000 messages/day
const REFRESH_SECS: u64 = 50 * 60; // SAS tokens last 1 hour
const TOKEN_TTL_SECS: u64 = 3600;

// ---- IoT Edge workload API ----

async fn workload_call(method: &str, path: &str, body: &str) -> Result<Value, BoxErr> {
    let uri = std::env::var("IOTEDGE_WORKLOADURI")?;
    let socket = uri.strip_prefix("unix://").ok_or("unexpected workload URI")?;
    let mut stream = UnixStream::connect(socket).await?;
    // HTTP/1.0 keeps the response free of chunked encoding
    let req = format!(
        "{method} {path} HTTP/1.0\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(req.as_bytes()).await?;
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await?;
    let text = String::from_utf8(raw)?;
    let (head, payload) = text.split_once("\r\n\r\n").ok_or("malformed response")?;
    let status_line = head.lines().next().unwrap_or("");
    if !status_line.contains(" 200 ") {
        return Err(format!("workload API: {status_line}: {payload}").into());
    }
    Ok(serde_json::from_str(payload)?)
}

async fn module_sas_token() -> Result<String, BoxErr> {
    let hub = std::env::var("IOTEDGE_IOTHUBHOSTNAME")?;
    let device = std::env::var("IOTEDGE_DEVICEID")?;
    let module = std::env::var("IOTEDGE_MODULEID")?;
    let genid = std::env::var("IOTEDGE_MODULEGENERATIONID")?;
    let api = std::env::var("IOTEDGE_APIVERSION")?;

    let expiry = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + TOKEN_TTL_SECS;
    let resource =
        urlencoding::encode(&format!("{hub}/devices/{device}/modules/{module}")).into_owned();
    let to_sign = format!("{resource}\n{expiry}");

    let body = json!({
        "keyId": "primary",
        "algo": "HMACSHA256",
        "data": STANDARD.encode(to_sign)
    })
    .to_string();
    let path = format!("/modules/{module}/genid/{genid}/sign?api-version={api}");
    let resp = workload_call("POST", &path, &body).await?;
    let digest = resp["digest"].as_str().ok_or("no digest in response")?;
    Ok(format!(
        "SharedAccessSignature sr={resource}&sig={}&se={expiry}",
        urlencoding::encode(digest)
    ))
}

async fn trust_bundle() -> Result<String, BoxErr> {
    let api = std::env::var("IOTEDGE_APIVERSION")?;
    let resp = workload_call("GET", &format!("/trust-bundle?api-version={api}"), "").await?;
    Ok(resp["certificate"]
        .as_str()
        .ok_or("no certificate in response")?
        .to_string())
}

// ---- edgeHub MQTT connection with token refresh ----

async fn connect_edge_hub() -> Result<(AsyncClient, rumqttc::EventLoop), BoxErr> {
    let hub = std::env::var("IOTEDGE_IOTHUBHOSTNAME")?;
    let device = std::env::var("IOTEDGE_DEVICEID")?;
    let module = std::env::var("IOTEDGE_MODULEID")?;
    let gateway = std::env::var("IOTEDGE_GATEWAYHOSTNAME")?;

    let token = module_sas_token().await?;
    let ca = trust_bundle().await?;

    let mut opts = MqttOptions::new(format!("{device}/{module}"), gateway, 8883);
    opts.set_credentials(format!("{hub}/{device}/{module}/?api-version=2018-06-30"), token);
    opts.set_keep_alive(Duration::from_secs(30));
    opts.set_transport(Transport::tls_with_config(TlsConfiguration::Simple {
        ca: ca.into_bytes(),
        alpn: None,
        client_auth: None,
    }));

    Ok(AsyncClient::new(opts, 10))
}

/// Keeps a live edgeHub connection in `shared`, rebuilding it with a fresh
/// token before the old one expires and after any connection error.
async fn edge_hub_supervisor(shared: Shared) {
    loop {
        match connect_edge_hub().await {
            Ok((client, mut eventloop)) => {
                *shared.lock().unwrap() = Some(client);
                let deadline = tokio::time::Instant::now() + Duration::from_secs(REFRESH_SECS);
                loop {
                    tokio::select! {
                        _ = tokio::time::sleep_until(deadline) => {
                            println!("refreshing edgeHub connection (new token)");
                            break;
                        }
                        ev = eventloop.poll() => match ev {
                            Ok(Event::Incoming(Packet::ConnAck(_))) => println!("connected to edgeHub"),
                            Ok(_) => {}
                            Err(e) => {
                                eprintln!("mqtt: {e}");
                                break;
                            }
                        },
                    }
                }
                *shared.lock().unwrap() = None;
            }
            Err(e) => eprintln!("edgeHub setup failed: {e}"),
        }
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

// ---- Serial reader ----

async fn read_port(path: &'static str, latest: Latest) -> Result<(), BoxErr> {
    let port = tokio_serial::new(path, SERIAL_BAUD).open_native_async()?;
    let mut lines = BufReader::new(port).lines();

    while let Some(line) = lines.next_line().await? {
        let Ok(reading) = serde_json::from_str::<Value>(&line) else {
            continue; // ignore non-JSON lines
        };
        if reading.get("error").is_some() {
            eprintln!("{path}: sensor error: {reading}");
            continue;
        }
        if let Some(node) = reading.get("node").and_then(Value::as_str).map(String::from) {
            latest.lock().unwrap().insert(node, (reading, Instant::now()));
        }
    }
    Ok(())
}

// ---- Main ----

#[tokio::main]
async fn main() {
    let edge = std::env::var("IOTEDGE_WORKLOADURI").is_ok();
    println!(
        "mercury-rust starting ({})",
        if edge { "IoT Edge" } else { "local, print only" }
    );

    let shared: Shared = Default::default();
    if edge {
        tokio::spawn(edge_hub_supervisor(shared.clone()));
    }

    let topic = format!(
        "devices/{}/modules/{}/messages/events/$.ct=application%2Fjson&$.ce=utf-8",
        std::env::var("IOTEDGE_DEVICEID").unwrap_or_default(),
        std::env::var("IOTEDGE_MODULEID").unwrap_or_default(),
    );

    let latest: Latest = Default::default();
    let l = latest.clone();
    tokio::spawn(async move {
        if let Err(e) = read_port(SERIAL_PORT, l).await {
            eprintln!("reader stopped: {e}");
        }
    });

    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    let mut sigint = signal(SignalKind::interrupt()).unwrap();

    let mut tick = tokio::time::interval(Duration::from_secs(SEND_INTERVAL_SECS));
    loop {
        tokio::select! {
            _ = tick.tick() => {}
            _ = sigterm.recv() => break,
            _ = sigint.recv() => break,
        }

        let payload = {
            let map = latest.lock().unwrap();
            if map.is_empty() {
                continue; // nothing to send yet
            }
            let mut out = serde_json::Map::new();
            for (node, (reading, seen)) in map.iter() {
                let mut r = reading.clone();
                r["age_sec"] = json!(seen.elapsed().as_secs());
                out.insert(node.clone(), r);
            }
            Value::Object(out)
        };

        if !edge {
            println!("{payload}");
            continue;
        }

        let client = shared.lock().unwrap().clone();
        match client {
            Some(c) => match c
                .publish(topic.as_str(), QoS::AtLeastOnce, false, payload.to_string())
                .await
            {
                Ok(()) => println!("published: {payload}"),
                Err(e) => eprintln!("publish failed: {e}"),
            },
            None => eprintln!("edgeHub not connected, skipping this reading"),
        }
    }
}