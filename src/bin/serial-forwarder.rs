use std::env;
use std::io::{self, BufRead, BufReader};
use std::time::Duration;

use log::{error, info};
use serialport::SerialPort;
use tungstenite::{connect, Message, stream::MaybeTlsStream};
use std::net::TcpStream;

type WebSocketStream = tungstenite::WebSocket<MaybeTlsStream<TcpStream>>;

fn main() {
    let port_path = match env::args().nth(1) {
        Some(p) => p,
        None => {
            eprintln!("Usage: serial-forwarder <serial-port-path>");
            std::process::exit(1);
        }
    };

    info!("Opening serial port: {}", port_path);

    let port = match serialport::new(&port_path, 115_200)
        .timeout(Duration::from_millis(500))
        .open()
    {
        Ok(p) => p,
        Err(e) => {
            error!("Failed to open serial port {}: {}", port_path, e);
            std::process::exit(1);
        }
    };

    info!("Serial port opened successfully.");

    // We keep the websocket optional and establish it on first valid request line.
    let mut maybe_ws = None;

    if let Err(e) = read_and_forward(port, &mut maybe_ws) {
        error!("Error while running forwarder: {}", e);
    }

    // Drop websocket connection explicitly on exit.
    if let Some(mut ws) = maybe_ws {
        info!("Closing websocket connection before exit.");
        let _ = ws.close(None);
    }

    info!("Exiting serial-forwarder.");
}

fn read_and_forward(
    port: Box<dyn SerialPort>,
    maybe_ws: &mut Option<WebSocketStream>,
) -> io::Result<()> {
    let mut reader = BufReader::new(port);
    let mut line = String::new();

    loop {
        line.clear();
        let bytes_read = reader.read_line(&mut line)?;

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
                        match connect_device_ws(&short_sn) {
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
                        if let Err(e) = ws.send(Message::Binary(bytes.clone().into())) {
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
        let byte =
            u8::from_str_radix(&byte_str, 16).map_err(|e| format!("invalid hex '{}': {}", byte_str, e))?;
        bytes.push(byte);
    }

    Ok(bytes)
}

fn connect_device_ws(
    short_sn: &str,
) -> Result<WebSocketStream, Box<dyn std::error::Error>> {
    let url_str = format!("ws://localhost:3000/ws/devices/{}", short_sn);
    info!("Connecting websocket to {}", url_str);

    let (mut socket, _response) = connect(&url_str)?;

    info!("Websocket connected for device {}", short_sn);

    // Optionally, we could perform a ping here to ensure connection is alive.
    if let Err(e) = socket.send(Message::Ping(vec![].into())) {
        error!("Websocket ping failed right after connect: {}", e);
    }

    Ok(socket)
}
