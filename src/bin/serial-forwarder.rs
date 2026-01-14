use std::collections::HashSet;
use std::io::{self as std_io, Write};
use std::net::IpAddr;
use std::time::Duration;

use chrono::Local;
use colored::*;
use futures_util::{pin_mut, SinkExt, StreamExt};
use serialport::{available_ports, SerialPortInfo, SerialPortType};
use tokio::fs::File as TokioFile;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, watch};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio_tungstenite::{connect_async, tungstenite::Message};

type WebSocketStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

type WsBinaryTx = mpsc::UnboundedSender<Vec<u8>>;
type DoneTx = mpsc::UnboundedSender<String>;

type AnyError = Box<dyn std::error::Error + Send + Sync>;

const SCAN_INTERVAL_SECS: u64 = 5;
const HEARTBEAT_INTERVAL_SECS: u64 = 5;
const SHORT_SN_RESPONSE_TIMEOUT_SECS: u64 = 3;

#[tokio::main]
async fn main() {
    // Enable ANSI color support on Windows consoles (no-op on other platforms).
    #[cfg(windows)]
    {
        let _ = colored::control::set_virtual_terminal(true);
    }

    let server_url = discover_server_with_retry().await;
    run_discovery_loop(server_url).await;
}

async fn discover_server_with_retry() -> String {
    loop {
        match discover_server().await {
            Ok(url) => {
                println!("{} {}", "已发现裁判系统服务器:".bold().green(), url.cyan());
                return url;
            }
            Err(e) => {
                eprintln!(
                    "{}",
                    format!(
                        "搜索裁判系统服务器失败：{}，将在 {} 秒后重试...",
                        e, SCAN_INTERVAL_SECS
                    )
                    .yellow()
                    .bold()
                );
                tokio::time::sleep(Duration::from_secs(SCAN_INTERVAL_SECS)).await;
            }
        }
    }
}

async fn run_discovery_loop(server_url: String) {
    let (done_tx, mut done_rx) = mpsc::unbounded_channel::<String>();
    let mut active_ports: HashSet<String> = HashSet::new();
    let mut declined_ports: HashSet<String> = HashSet::new();

    let mut ticker = tokio::time::interval(Duration::from_secs(SCAN_INTERVAL_SECS));

    loop {
        tokio::select! {
            _ = ticker.tick() => {
                let ports = match discover_usb_ports() {
                    Ok(p) => p,
                    Err(e) => {
                        eprintln!("{}", format!("枚举串口失败：{}", e).red().bold());
                        continue;
                    }
                };

                for port in ports {
                    if active_ports.contains(&port.port_name) {
                        continue;
                    }
                    if declined_ports.contains(&port.port_name) {
                        continue;
                    }

                    if !prompt_connect(&port).await {
                        declined_ports.insert(port.port_name.clone());
                        continue;
                    }

                    active_ports.insert(port.port_name.clone());
                    let server_url = server_url.clone();
                    let done_tx = done_tx.clone();
                    tokio::spawn(async move {
                        run_device_session(port, server_url, done_tx).await;
                    });
                }
            }
            Some(port) = done_rx.recv() => {
                active_ports.remove(&port);
            }
        }
    }
}

fn discover_usb_ports() -> Result<Vec<SerialPortInfo>, serialport::Error> {
    let ports = available_ports()?;
    Ok(ports
        .into_iter()
        .filter(|p| !is_ignored_port(p))
        .filter(|p| matches!(p.port_type, SerialPortType::UsbPort(_)))
        .collect())
}

#[cfg(target_os = "macos")]
fn is_ignored_port(port_info: &serialport::SerialPortInfo) -> bool {
    port_info.port_name.starts_with("/dev/tty.")
}

#[cfg(not(target_os = "macos"))]
fn is_ignored_port(_: &serialport::SerialPortInfo) -> bool {
    false
}

async fn prompt_connect(port: &SerialPortInfo) -> bool {
    let port = port.clone();
    tokio::task::spawn_blocking(move || {
        if let SerialPortType::UsbPort(info) = &port.port_type {
            println!(
                "\n{} {} (VID: 0x{:04X}, PID: 0x{:04X}, SN: {})",
                "发现新的USB串口设备:".bold().cyan(),
                port.port_name.cyan(),
                info.vid,
                info.pid,
                info.serial_number.as_deref().unwrap_or("未知").yellow()
            );
        } else {
            println!("\n{} {}", "发现设备:".bold().cyan(), port.port_name.cyan());
        }

        print!("{}", "是否连接此设备? [y/N] ".bold());
        std_io::stdout().flush().ok();

        let mut input = String::new();
        if std_io::stdin().read_line(&mut input).is_err() {
            return false;
        }

        matches!(input.trim().to_ascii_lowercase().as_str(), "y" | "yes")
    })
    .await
    .unwrap_or(false)
}

struct DoneGuard {
    port_name: String,
    done_tx: DoneTx,
}

impl Drop for DoneGuard {
    fn drop(&mut self) {
        let _ = self.done_tx.send(self.port_name.clone());
    }
}

#[derive(Debug)]
enum Event {
    ShortSn(String),
    ProtocolHex(String),
    SerialClosed,
    WsClosed,
}

struct WsHandle {
    tx: WsBinaryTx,
    shutdown: watch::Sender<bool>,
}

async fn run_device_session(port_info: SerialPortInfo, server_url: String, done_tx: DoneTx) {
    let port_name = port_info.port_name.clone();
    let _done = DoneGuard {
        port_name: port_name.clone(),
        done_tx,
    };

    let log_filename = make_log_filename(&port_info);
    let log_file = match TokioFile::create(&log_filename).await {
        Ok(f) => {
            println!("{} {}", "日志文件:".bold().green(), log_filename.cyan());
            f
        }
        Err(e) => {
            eprintln!(
                "{}",
                format!("创建设备日志文件失败（{}）：{}", log_filename, e).red().bold()
            );
            return;
        }
    };

    let port = match tokio_serial::new(&port_name, 115_200)
        .timeout(Duration::from_millis(500))
        .open_native_async()
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!(
                "{}",
                format!("打开串口 {} 失败：{}", port_name, e).red().bold()
            );
            return;
        }
    };

    println!("{} {}", "串口打开成功:".bold().green(), port_name.cyan());

    if let Err(e) = session_loop(port, server_url, log_file).await {
        eprintln!(
            "{}",
            format!("串口转发运行出错（{}）：{}", port_name, e).red().bold()
        );
    }

    println!(
        "{} {}",
        "串口转发任务已结束:".bold().yellow(),
        port_name.cyan()
    );
}

fn make_log_filename(port_info: &SerialPortInfo) -> String {
    let serial_number = match &port_info.port_type {
        SerialPortType::UsbPort(info) => info
            .serial_number
            .as_deref()
            .unwrap_or("UNKNOWN")
            .to_string(),
        _ => "UNKNOWN".to_string(),
    };

    let sanitized_serial = serial_number.replace(':', "");
    let now = Local::now();
    format!(
        "logs_{}_{}.txt",
        sanitized_serial,
        now.format("%Y-%m-%d-%H-%M-%S")
    )
}

async fn session_loop(
    port: SerialStream,
    server_url: String,
    log_file: TokioFile,
) -> Result<(), AnyError> {
    let (read_half, write_half) = io::split(port);

    let (event_tx, mut event_rx) = mpsc::unbounded_channel::<Event>();
    let (serial_write_tx, serial_write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(serial_writer_task(
        write_half,
        serial_write_rx,
        shutdown_rx.clone(),
    ));
    tokio::spawn(serial_reader_task(
        read_half,
        event_tx.clone(),
        log_file,
        shutdown_rx.clone(),
    ));

    let mut ws: Option<WsHandle> = None;
    let mut current_short_sn: Option<String> = None;

    // 连接后立即心跳一次
    serial_write_tx.send(b"referee heartbeat\n".to_vec()).ok();
    let mut waiting_short_sn = true;

    let mut heartbeat = tokio::time::interval(Duration::from_secs(HEARTBEAT_INTERVAL_SECS));
    heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    loop {
        if waiting_short_sn {
            tokio::select! {
                _ = tokio::time::sleep(Duration::from_secs(SHORT_SN_RESPONSE_TIMEOUT_SECS)) => {
                    eprintln!(
                        "{}",
                        format!(
                            "超过 {} 秒未收到新的 SerialProtocolShortSN，正在断开并关闭串口。",
                            SHORT_SN_RESPONSE_TIMEOUT_SECS
                        ).yellow().bold()
                    );
                    break;
                }
                Some(ev) = event_rx.recv() => {
                    if handle_event(
                        ev,
                        &server_url,
                        &serial_write_tx,
                        &mut ws,
                        &mut current_short_sn,
                        &mut waiting_short_sn,
                        &event_tx,
                    ).await? {
                        break;
                    }
                }
            }

            continue;
        }

        tokio::select! {
            _ = heartbeat.tick() => {
                serial_write_tx.send(b"referee heartbeat\n".to_vec()).ok();
                waiting_short_sn = true;
            }
            Some(ev) = event_rx.recv() => {
                if handle_event(
                    ev,
                    &server_url,
                    &serial_write_tx,
                    &mut ws,
                    &mut current_short_sn,
                    &mut waiting_short_sn,
                    &event_tx,
                ).await? {
                    break;
                }
            }
        }
    }

    if let Some(ws) = ws.take() {
        let _ = ws.shutdown.send(true);
    }
    let _ = shutdown_tx.send(true);
    Ok(())
}

async fn handle_event(
    ev: Event,
    server_url: &str,
    serial_write_tx: &mpsc::UnboundedSender<Vec<u8>>,
    ws: &mut Option<WsHandle>,
    current_short_sn: &mut Option<String>,
    waiting_short_sn: &mut bool,
    event_tx: &mpsc::UnboundedSender<Event>,
) -> Result<bool, AnyError> {
    match ev {
        Event::SerialClosed => return Ok(true),
        Event::WsClosed => {
            println!(
                "{}",
                "由于 websocket 断开，停止串口读取循环。".yellow().bold()
            );
            if let Some(ws) = ws.take() {
                let _ = ws.shutdown.send(true);
            }
        }
        Event::ShortSn(sn) => {
            *waiting_short_sn = false;
            if current_short_sn.as_deref() != Some(&sn) {
                *current_short_sn = Some(sn.clone());
            }

            if ws.is_none() {
                match connect_device_ws(&sn, server_url).await {
                    Ok(socket) => {
                        let handle =
                            spawn_ws_tasks(socket, serial_write_tx.clone(), event_tx.clone());
                        *ws = Some(handle);
                    }
                    Err(e) => {
                        eprintln!("{}", format!("WS 连接失败: {}", e).red().bold());
                    }
                }
            }
        }
        Event::ProtocolHex(hex) => {
            let Some(ws) = ws.as_ref() else {
                return Ok(false);
            };

            let bytes = match hex_to_bytes(&hex) {
                Ok(b) => b,
                Err(_) => return Ok(false),
            };

            let _ = ws.tx.send(bytes);
        }
    }

    Ok(false)
}

async fn serial_writer_task(
    mut write_half: tokio::io::WriteHalf<SerialStream>,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut shutdown: watch::Receiver<bool>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            msg = rx.recv() => {
                let Some(bytes) = msg else { break; };
                if write_half.write_all(&bytes).await.is_err() {
                    break;
                }
            }
        }
    }
}

async fn serial_reader_task(
    read_half: tokio::io::ReadHalf<SerialStream>,
    event_tx: mpsc::UnboundedSender<Event>,
    mut log_file: TokioFile,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut reader = BufReader::new(read_half);
    let mut line = String::new();

    loop {
        line.clear();
        tokio::select! {
            _ = shutdown.changed() => break,
            res = reader.read_line(&mut line) => {
                match res {
                    Ok(0) => {
                        println!("{}", "串口已关闭。".yellow().bold());
                        let _ = event_tx.send(Event::SerialClosed);
                        break;
                    }
                    Ok(_) => {
                        let _ = log_file.write_all(line.as_bytes()).await;

                        let trimmed = line.trim_end_matches(['\r', '\n']);
                        if trimmed.is_empty() {
                            continue;
                        }

                        if let Some(sn) = extract_short_sn(trimmed) {
                            let _ = event_tx.send(Event::ShortSn(sn));
                        }
                        if let Some(hex) = extract_protocol_hex(trimmed) {
                            let _ = event_tx.send(Event::ProtocolHex(hex.to_string()));
                        }
                    }
                    Err(_) => {
                        let _ = event_tx.send(Event::SerialClosed);
                        break;
                    }
                }
            }
        }
    }
}

fn spawn_ws_tasks(
    socket: WebSocketStream,
    serial_write_tx: mpsc::UnboundedSender<Vec<u8>>,
    event_tx: mpsc::UnboundedSender<Event>,
) -> WsHandle {
    let (ws_tx, ws_rx) = socket.split();

    let (bin_tx, bin_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    let (shutdown_tx, shutdown_rx) = watch::channel(false);

    tokio::spawn(ws_send_task(
        ws_tx,
        bin_rx,
        shutdown_rx.clone(),
        event_tx.clone(),
    ));
    tokio::spawn(ws_recv_task(
        ws_rx,
        serial_write_tx,
        shutdown_rx.clone(),
        event_tx,
    ));

    WsHandle {
        tx: bin_tx,
        shutdown: shutdown_tx,
    }
}

async fn ws_send_task(
    mut ws_tx: futures_util::stream::SplitSink<WebSocketStream, Message>,
    mut bin_rx: mpsc::UnboundedReceiver<Vec<u8>>,
    mut shutdown: watch::Receiver<bool>,
    event_tx: mpsc::UnboundedSender<Event>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            msg = bin_rx.recv() => {
                let Some(bytes) = msg else { break; };
                if ws_tx.send(Message::Binary(bytes.into())).await.is_err() {
                    let _ = event_tx.send(Event::WsClosed);
                    break;
                }
            }
        }
    }
}

async fn ws_recv_task(
    mut ws_rx: futures_util::stream::SplitStream<WebSocketStream>,
    serial_write_tx: mpsc::UnboundedSender<Vec<u8>>,
    mut shutdown: watch::Receiver<bool>,
    event_tx: mpsc::UnboundedSender<Event>,
) {
    loop {
        tokio::select! {
            _ = shutdown.changed() => break,
            msg = ws_rx.next() => {
                let Some(msg) = msg else {
                    let _ = event_tx.send(Event::WsClosed);
                    break;
                };

                match msg {
                    Ok(Message::Binary(data)) => {
                        let hex: String = data.iter().map(|b| format!("{:02X}", b)).collect();
                        let line = format!("referee send {}\n", hex);
                        if serial_write_tx.send(line.into_bytes()).is_err() {
                            break;
                        }
                    }
                    Ok(Message::Close(frame)) => {
                        println!(
                            "{} {:?}",
                            "websocket连接被服务器关闭：".yellow().bold(),
                            frame
                        );
                        let _ = event_tx.send(Event::WsClosed);
                        break;
                    }
                    Ok(_) => {}
                    Err(_) => {
                        let _ = event_tx.send(Event::WsClosed);
                        break;
                    }
                }
            }
        }
    }
}

/// Extracts the hex payload inside `FOX-RefereeSystem: SerialProtocolRequest[...]`.
fn extract_protocol_hex(line: &str) -> Option<&str> {
    const PREFIX: &str = "FOX-RefereeSystem: SerialProtocolRequest[";
    let start = line.find(PREFIX)?;
    let after_prefix = &line[start + PREFIX.len()..];
    let end_idx = after_prefix.find(']')?;
    let hex_str = &after_prefix[..end_idx];
    if hex_str.is_empty() {
        None
    } else {
        Some(hex_str)
    }
}

/// Extracts the short serial number inside `SerialProtocolShortSN[...]` if present.
fn extract_short_sn(line: &str) -> Option<String> {
    const PREFIX: &str = "SerialProtocolShortSN[";
    let start = line.find(PREFIX)?;
    let after_prefix = &line[start + PREFIX.len()..];
    let end_idx = after_prefix.find(']')?;
    let sn = after_prefix[..end_idx].trim();
    if sn.is_empty() {
        None
    } else {
        Some(sn.to_string())
    }
}

fn hex_to_bytes(hex: &str) -> Result<Vec<u8>, String> {
    if hex.len() % 2 != 0 {
        return Err("hex length must be even".into());
    }

    let mut bytes = Vec::with_capacity(hex.len() / 2);
    let chars: Vec<char> = hex.chars().collect();

    for i in (0..chars.len()).step_by(2) {
        let byte_str = format!("{}{}", chars[i], chars[i + 1]);
        let byte = u8::from_str_radix(&byte_str, 16)
            .map_err(|e| format!("invalid hex '{}': {}", byte_str, e))?;
        bytes.push(byte);
    }

    Ok(bytes)
}

async fn discover_server() -> Result<String, AnyError> {
    const SERVICE_TYPE: &str = "_http._tcp.local";
    const INSTANCE_NAME: &str = "RadioMaster Referee System";

    println!("{}", "正在搜索裁判系统服务器...".bold().cyan());

    let stream = mdns::discover::all(SERVICE_TYPE, Duration::from_secs(10))?.listen();
    pin_mut!(stream);

    while let Some(Ok(response)) = stream.next().await {
        let Some(ptr_name) = response.hostname() else {
            continue;
        };

        if !ptr_name.starts_with(INSTANCE_NAME) || !ptr_name.ends_with(SERVICE_TYPE) {
            continue;
        }

        if let Some(socket_addr) = response.socket_address() {
            let host = match socket_addr.ip() {
                IpAddr::V4(addr) => addr.to_string(),
                IpAddr::V6(addr) => format!("[{}]", addr),
            };
            return Ok(format!("ws://{}:{}", host, socket_addr.port()));
        }

        if let Some(addr) = response.ip_addr() {
            let host = match addr {
                IpAddr::V4(addr) => addr.to_string(),
                IpAddr::V6(addr) => format!("[{}]", addr),
            };
            return Ok(format!("ws://{}:3000", host));
        }
    }

    Err("没找到裁判系统".into())
}

async fn connect_device_ws(short_sn: &str, server_url: &str) -> Result<WebSocketStream, AnyError> {
    let url_str = format!("{}/ws/devices/{}", server_url, short_sn);
    println!(
        "{} {}",
        "正在建立websocket连接:".bold().cyan(),
        url_str.cyan()
    );

    let (mut socket, _response) = connect_async(&url_str).await?;
    let _ = socket.send(Message::Ping(vec![].into())).await;

    println!(
        "{}",
        "websocket已连接，消息转发开始".bold().green()
    );

    Ok(socket)
}
