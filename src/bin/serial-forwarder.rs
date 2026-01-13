use std::collections::HashSet;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use futures_util::{pin_mut, SinkExt, StreamExt};
use log::{error, info, warn};
use serialport::{available_ports, SerialPortType};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::Mutex;
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio_tungstenite::{connect_async, tungstenite::Message};

type WebSocketStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

#[tokio::main]
async fn main() {
    const TARGET_VID: u16 = 0x303A;
    const SCAN_INTERVAL_SECS: u64 = 5;

    env_logger::init();

    info!(
        "Starting serial port discovery for USB VID 0x{:04X}...",
        TARGET_VID
    );

    // Discover server via mDNS (keep retrying until found)
    let server_url = loop {
        match discover_server().await {
            Ok(url) => {
                info!("Discovered server: {}", url);
                break Arc::new(url);
            }
            Err(e) => {
                warn!(
                    "Failed to discover server via mDNS: {}. Retrying in {} seconds...",
                    e, SCAN_INTERVAL_SECS
                );
                tokio::time::sleep(Duration::from_secs(SCAN_INTERVAL_SECS)).await;
            }
        }
    };

    // Track which ports are currently being handled
    let active_ports = Arc::new(Mutex::new(HashSet::<String>::new()));

    // Continuous discovery loop
    loop {
        // List all available serial ports
        let ports = match available_ports() {
            Ok(ports) => ports,
            Err(e) => {
                error!("Failed to list serial ports: {}", e);
                tokio::time::sleep(Duration::from_secs(SCAN_INTERVAL_SECS)).await;
                continue;
            }
        };

        // Filter for USB ports with matching VID
        let matching_ports: Vec<_> = ports
            .into_iter()
            .filter_map(|port_info| match &port_info.port_type {
                SerialPortType::UsbPort(usb_info) => {
                    if usb_info.vid == TARGET_VID {
                        info!(
                            "Found matching USB port: {} (VID: 0x{:04X}, PID: 0x{:04X})",
                            port_info.port_name, usb_info.vid, usb_info.pid
                        );
                        Some(port_info.port_name)
                    } else {
                        None
                    }
                }
                _ => None,
            })
            .collect();

        if matching_ports.is_empty() {
            info!(
                "No USB serial ports found with VID 0x{:04X}. Continuing to scan...",
                TARGET_VID
            );
        } else {
            info!("Found {} matching USB port(s)", matching_ports.len());
        }

        // Check which ports are new and need to be handled
        let mut active = active_ports.lock().await;
        for port_path in matching_ports {
            if active.contains(&port_path) {
                continue; // Already handling this port
            }

            // Mark as active
            active.insert(port_path.clone());
            let port_path_clone = port_path.clone();
            let server_url_clone = Arc::clone(&server_url);
            let active_ports_clone = Arc::clone(&active_ports);

            // Spawn task to handle this port
            tokio::spawn(async move {
                info!("Opening serial port: {}", port_path_clone);

                let port = match tokio_serial::new(&port_path_clone, 115_200)
                    .timeout(Duration::from_millis(500))
                    .open_native_async()
                {
                    Ok(p) => p,
                    Err(e) => {
                        error!("Failed to open serial port {}: {}", port_path_clone, e);
                        // Remove from active ports on failure
                        let mut active = active_ports_clone.lock().await;
                        active.remove(&port_path_clone);
                        return;
                    }
                };

                info!("Serial port {} opened successfully.", port_path_clone);

                // We keep the websocket optional and establish it on first valid request line.
                let mut maybe_ws = None;

                if let Err(e) = read_and_forward(port, &mut maybe_ws, &server_url_clone).await {
                    error!(
                        "Error while running forwarder on {}: {}",
                        port_path_clone, e
                    );
                }

                // Drop websocket connection explicitly on exit.
                if let Some(mut ws) = maybe_ws {
                    info!(
                        "Closing websocket connection for {} before exit.",
                        port_path_clone
                    );
                    let _ = ws.close(None).await;
                }

                // Remove from active ports when done
                let mut active = active_ports_clone.lock().await;
                active.remove(&port_path_clone);
                info!("Exiting forwarder task for {}.", port_path_clone);
            });
        }
        drop(active); // Release the lock

        // Wait before next scan
        tokio::time::sleep(Duration::from_secs(SCAN_INTERVAL_SECS)).await;
    }
}

async fn read_and_forward(
    port: SerialStream,
    maybe_ws: &mut Option<WebSocketStream>,
    server_url: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let mut reader = BufReader::new(port);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;

        if bytes_read == 0 {
            // Serial port closed.
            info!("Serial port closed (EOF).");
            break;
        }

        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if trimmed.is_empty() {
            continue;
        }

        info!("Received line from serial: {}", trimmed);

        if let Some(hex_str) = extract_protocol_hex(trimmed) {
            match hex_to_bytes(hex_str) {
                Ok(bytes) => {
                    if bytes.len() < 4 {
                        error!(
                            "Protocol payload too short ({} bytes), skipping.",
                            bytes.len()
                        );
                        continue;
                    }

                    // shortSN is bytes[2..4], formatted as 4-char uppercase hex, consistent with server.
                    let short_sn = format!("{:02X}{:02X}", bytes[2], bytes[3]);
                    info!("Parsed shortSN: {}", short_sn);

                    // Ensure websocket is connected.
                    if maybe_ws.is_none() {
                        match connect_device_ws(&short_sn, server_url).await {
                            Ok(ws) => {
                                *maybe_ws = Some(ws);
                            }
                            Err(e) => {
                                error!("Failed to establish websocket: {}", e);
                                // If we cannot connect, stop processing.
                                break;
                            }
                        }
                    }

                    if let Some(ws) = maybe_ws.as_mut() {
                        info!(
                            "Forwarding protocol message ({} bytes) to websocket for shortSN {}",
                            bytes.len(),
                            short_sn
                        );
                        if let Err(e) = ws.send(Message::Binary(bytes.clone().into())).await {
                            error!("Failed to send message over websocket: {}", e);
                            // On websocket error, break; main will close and exit.
                            break;
                        }
                    }
                }
                Err(e) => {
                    error!("Failed to parse hex payload '{}': {}", hex_str, e);
                }
            }
        }
    }

    Ok(())
}

/// Extracts the hex payload inside `FOX-RefereeSystem: SerialProtocolRequest[...]`.
fn extract_protocol_hex(line: &str) -> Option<&str> {
    const PREFIX: &str = "FOX-RefereeSystem: SerialProtocolRequest[";
    if let Some(start) = line.find(PREFIX) {
        let after_prefix = &line[start + PREFIX.len()..];
        if let Some(end_idx) = after_prefix.find(']') {
            let hex_str = &after_prefix[..end_idx];
            if !hex_str.is_empty() {
                return Some(hex_str);
            }
        }
    }
    None
}

/// Convert a hex string (e.g. "AAA5BABD01") to bytes.
fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 {
        return Err("hex string must have even length".into());
    }

    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let chars: Vec<char> = hex.chars().collect();

    for i in (0..chars.len()).step_by(2) {
        let hi = chars[i];
        let lo = chars[i + 1];
        let byte_str = format!("{}{}", hi, lo);
        let byte = u8::from_str_radix(&byte_str, 16)
            .map_err(|e| format!("invalid hex '{}': {}", byte_str, e))?;
        bytes.push(byte);
    }

    Ok(bytes)
}

/// Discovers the "RadioMaster Referee System" HTTP server via mDNS
async fn discover_server() -> Result<String, Box<dyn std::error::Error>> {
    const SERVICE_TYPE: &str = "_http._tcp.local";
    const INSTANCE_NAME: &str = "RadioMaster Referee System";

    info!(
        "Searching for mDNS service instance '{}' ({})",
        INSTANCE_NAME, SERVICE_TYPE
    );

    // Browse for HTTP services and filter by instance name.
    let stream = mdns::discover::all(SERVICE_TYPE, Duration::from_secs(10))?.listen();
    pin_mut!(stream);

    while let Some(Ok(response)) = stream.next().await {
        // Ensure this response is for our expected instance.
        let Some(ptr_name) = response.hostname() else {
            continue;
        };

        if !ptr_name.starts_with(INSTANCE_NAME) || !ptr_name.ends_with(SERVICE_TYPE) {
            continue;
        }

        // Prefer SRV-advertised port when available; fall back to port 3000 otherwise.
        if let Some(socket_addr) = response.socket_address() {
            let host = match socket_addr.ip() {
                IpAddr::V4(addr) => addr.to_string(),
                IpAddr::V6(addr) => format!("[{}]", addr),
            };
            let url = format!("http://{}:{}", host, socket_addr.port());
            info!("Discovered server URL via SRV: {}", url);
            return Ok(url);
        }

        if let Some(addr) = response.ip_addr() {
            let host = match addr {
                IpAddr::V4(addr) => addr.to_string(),
                IpAddr::V6(addr) => format!("[{}]", addr),
            };
            let url = format!("http://{}:3000", host);
            info!(
                "Discovered server URL (no SRV found), assuming port 3000: {}",
                url
            );
            return Ok(url);
        }
    }
    Err("Service not found via mDNS".into())
}

async fn connect_device_ws(
    short_sn: &str,
    server_url: &str,
) -> Result<WebSocketStream, Box<dyn std::error::Error>> {
    // Convert http:// to ws://
    let ws_url = if server_url.starts_with("http://") {
        server_url.replace("http://", "ws://")
    } else if server_url.starts_with("https://") {
        server_url.replace("https://", "wss://")
    } else {
        server_url.to_string()
    };

    let url_str = format!("{}/ws/devices/{}", ws_url, short_sn);
    info!("Connecting websocket to {}", url_str);

    let (mut socket, response) = connect_async(&url_str).await?;
    let _ = response;

    info!("Websocket connected for device {}", short_sn);

    // Optionally, we could perform a ping here to ensure connection is alive.
    if let Err(e) = socket.send(Message::Ping(vec![].into())).await {
        error!("Websocket ping failed right after connect: {}", e);
    }

    Ok(socket)
}
