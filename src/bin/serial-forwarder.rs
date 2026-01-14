use std::collections::HashSet;
use std::io::{self as std_io, Write};
use std::net::IpAddr;
use std::sync::Arc;
use std::time::Duration;

use chrono::Local;
use colored::*;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{pin_mut, SinkExt, StreamExt};
use serialport::{available_ports, SerialPortInfo, SerialPortType};
use tokio::fs::File as TokioFile;
use tokio::io::{self, AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{Mutex, Notify};
use tokio_serial::{SerialPortBuilderExt, SerialStream};
use tokio_tungstenite::{connect_async, tungstenite::Message};

type WebSocketStream =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;
type WebSocketSink = SplitSink<WebSocketStream, Message>;
type WebSocketRecvStream = SplitStream<WebSocketStream>;

const SCAN_INTERVAL_SECS: u64 = 5;

#[tokio::main]
async fn main() {
    let server_url = Arc::new(discover_server_with_retry().await);
    let active_ports = Arc::new(Mutex::new(HashSet::<String>::new()));

    run_discovery_loop(server_url, active_ports).await;
}

async fn discover_server_with_retry() -> String {
    loop {
        match discover_server().await {
            Ok(url) => {
                println!(
                    "{} {}",
                    "已发现裁判系统服务器:".bold().green(),
                    url.cyan()
                );
                break url;
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

async fn run_discovery_loop(
    server_url: Arc<String>,
    active_ports: Arc<Mutex<HashSet<String>>>,
) {
    loop {
        let matching_ports = match discover_matching_ports() {
            Ok(ports) => ports,
            Err(e) => {
                eprintln!("枚举串口失败：{}", e);
                tokio::time::sleep(Duration::from_secs(SCAN_INTERVAL_SECS)).await;
                continue;
            }
        };

        if matching_ports.is_empty() {
            println!(
                "{}",
                "未找到USB串口，继续扫描...".yellow().bold()
            );
        } else {
            spawn_new_port_handlers(&matching_ports, &server_url, &active_ports).await;
        }

        tokio::time::sleep(Duration::from_secs(SCAN_INTERVAL_SECS)).await;
    }
}

fn discover_matching_ports() -> Result<Vec<SerialPortInfo>, serialport::Error> {
    let ports = available_ports()?;

    let matching_ports: Vec<_> = ports
        .into_iter()
        .filter_map(|port_info| {
            if is_ignored_port(&port_info) {
                return None;
            }
            match &port_info.port_type {
                SerialPortType::UsbPort(_) => Some(port_info),
                _ => None,
            }
        })
        .collect();

    Ok(matching_ports)
}

#[cfg(target_os = "macos")]
fn is_ignored_port(port_info: &serialport::SerialPortInfo) -> bool {
    port_info.port_name.starts_with("/dev/tty.")
}

#[cfg(not(target_os = "macos"))]
fn is_ignored_port(_: &serialport::SerialPortInfo) -> bool {
    false
}

async fn spawn_new_port_handlers(
    matching_ports: &[SerialPortInfo],
    server_url: &Arc<String>,
    active_ports: &Arc<Mutex<HashSet<String>>>,
) {
    let mut active = active_ports.lock().await;
    let mut new_ports: Vec<SerialPortInfo> = Vec::new();

    for port_info in matching_ports {
        let port_path = &port_info.port_name;
        if active.contains(port_path) {
            continue;
        }
        new_ports.push(port_info.clone());
    }

    if new_ports.is_empty() {
        return;
    }

    // 让用户选择要连接的设备
    let selected_ports = select_ports_to_connect(&new_ports).await;
    
    if selected_ports.is_empty() {
        println!("{}", "未选择任何设备，跳过连接。".yellow().bold());
        return;
    }

    println!(
        "{} {}",
        "准备连接以下设备:".bold().green(),
        selected_ports.len().to_string().cyan()
    );

    for port_info in selected_ports {
        let port_path = port_info.port_name.clone();
        active.insert(port_path.clone());
        let port_path_clone = port_info.port_name.clone();
        let port_info_clone = port_info.clone();
        let server_url_clone = Arc::clone(server_url);
        let active_ports_clone = Arc::clone(active_ports);

        tokio::spawn(async move {
            println!(
                "{} {}",
                "打开串口:".bold().green(),
                port_path_clone.cyan()
            );

            // 为当前设备创建日志文件（使用设备 SN 命名），用于保存设备输出
            let serial_number = match &port_info_clone.port_type {
                SerialPortType::UsbPort(info) => info
                    .serial_number
                    .as_deref()
                    .unwrap_or("UNKNOWN")
                    .to_string(),
                _ => "UNKNOWN".to_string(),
            };

            // 去除 SN 中的冒号，避免 Windows 等系统的文件名问题
            let sanitized_serial = serial_number.replace(':', "");
            let now = Local::now();
            let log_filename = format!(
                "logs_{}_{}.txt",
                sanitized_serial,
                now.format("%Y-%m-%d-%H-%M-%S")
            );

            let log_file = match TokioFile::create(&log_filename).await {
                Ok(f) => {
                    println!(
                        "{} {}",
                        "设备日志将保存到文件:".bold().green(),
                        log_filename.cyan()
                    );
                    Some(Arc::new(Mutex::new(f)))
                }
                Err(e) => {
                    eprintln!(
                        "{}",
                        format!("创建设备日志文件失败（{}）：{}", log_filename, e)
                            .red()
                            .bold()
                    );
                    None
                }
            };

            let port = match tokio_serial::new(&port_path_clone, 115_200)
                .timeout(Duration::from_millis(500))
                .open_native_async()
            {
                Ok(p) => p,
                Err(e) => {
                    eprintln!(
                        "{}",
                        format!("打开串口 {} 失败：{}", port_path_clone, e)
                            .red()
                            .bold()
                    );
                    let mut active = active_ports_clone.lock().await;
                    active.remove(&port_path_clone);
                    return;
                }
            };

            println!(
                "{} {}",
                "串口打开成功:".bold().green(),
                port_path_clone.cyan()
            );

            if let Err(e) = read_and_forward(port, &server_url_clone, log_file).await {
                eprintln!(
                    "{}",
                    format!("串口转发运行出错（{}）：{}", port_path_clone, e)
                        .red()
                        .bold()
                );
            }

            let mut active = active_ports_clone.lock().await;
            active.remove(&port_path_clone);
            println!(
                "{} {}",
                "串口转发任务已结束:".bold().yellow(),
                port_path_clone.cyan()
            );
        });
    }
}

async fn select_ports_to_connect(ports: &[SerialPortInfo]) -> Vec<SerialPortInfo> {
    println!("\n{}", "发现新的USB串口设备:".bold().cyan());
    
    for (i, port) in ports.iter().enumerate() {
        if let SerialPortType::UsbPort(info) = &port.port_type {
            println!(
                "  {}: {} (VID: 0x{:04X}, PID: 0x{:04X}, SN: {})",
                i,
                port.port_name.cyan(),
                info.vid,
                info.pid,
                info.serial_number.as_deref().unwrap_or("未知").yellow()
            );
        } else {
            println!("  {}: {}", i, port.port_name.cyan());
        }
    }

    loop {
        print!("{}", "请选择要连接的设备编号（用逗号分隔，如 0,1 或输入 'all' 连接全部，'skip' 跳过）: ".bold());
        std_io::stdout().flush().ok();

        let mut input = String::new();
        if std_io::stdin().read_line(&mut input).is_err() {
            return Vec::new();
        }

        let trimmed = input.trim();
        
        if trimmed.eq_ignore_ascii_case("all") {
            return ports.to_vec();
        }
        
        if trimmed.eq_ignore_ascii_case("skip") {
            return Vec::new();
        }

        let mut selected = Vec::new();
        let mut valid = true;
        
        for part in trimmed.split(',') {
            let part = part.trim();
            match part.parse::<usize>() {
                Ok(idx) if idx < ports.len() => {
                    selected.push(ports[idx].clone());
                }
                _ => {
                    println!(
                        "{}",
                        format!("无效选择 '{}'。请输入 0 到 {} 之间的数字，用逗号分隔。", part, ports.len() - 1)
                            .yellow()
                            .bold()
                    );
                    valid = false;
                    break;
                }
            }
        }

        if valid && !selected.is_empty() {
            return selected;
        }
    }
}

async fn read_and_forward(
    port: SerialStream,
    server_url: &str,
    log_file: Option<Arc<Mutex<TokioFile>>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let (read_half, write_half) = io::split(port);
    let writer = Arc::new(Mutex::new(write_half));
    let shutdown = Arc::new(Notify::new());
    spawn_heartbeat_task(&writer, &shutdown);

    let mut reader = BufReader::new(read_half);
    let mut maybe_ws: Option<WebSocketSink> = None;
    let mut line = String::new();
    // 最近一次从日志中解析到的 SerialProtocolShortSN，用于后续的协议报文
    let mut last_short_sn: Option<String> = None;

    loop {
        line.clear();
        tokio::select! {
            res = reader.read_line(&mut line) => {
                let bytes_read = res?;

                if bytes_read == 0 {
                    println!("{}", "串口已关闭（EOF）。".yellow().bold());
                    if let Some(ws) = maybe_ws.as_mut() {
                        if let Err(e) = ws.close().await {
                            eprintln!(
                                "{}",
                                format!("串口关闭后关闭 websocket 失败：{}", e)
                                    .red()
                                    .bold()
                            );
                        }
                    }
                    shutdown.notify_waiters();
                    break;
                }

                let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
                if trimmed.is_empty() {
                    continue;
                }

                // 将设备原始日志写入文件
                if let Some(f) = &log_file {
                    let mut guard = f.lock().await;
                    // 写入原始行（包含换行符）
                    if let Err(e) = guard.write_all(line.as_bytes()).await {
                        eprintln!(
                            "{}",
                            format!("写入设备日志文件失败：{}", e)
                                .red()
                                .bold()
                        );
                    }
                }

                // 先尝试在当前日志行中提取 SerialProtocolShortSN，用于后续协议报文
                if let Some(sn) = extract_short_sn(trimmed) {
                    last_short_sn = Some(sn);
                }

                if let Some(hex_str) = extract_protocol_hex(trimmed) {
                    // 必须已经从之前的日志行中拿到短 SN，否则忽略该报文
                    let short_sn = match &last_short_sn {
                        Some(sn) => sn.clone(),
                        None => {
                            eprintln!(
                                "{}",
                                "未在之前的日志中找到 SerialProtocolShortSN，已忽略该协议报文。"
                                    .yellow()
                                    .bold()
                            );
                            continue;
                        }
                    };

                    if let Err(e) = handle_protocol_line(
                        &short_sn,
                        hex_str,
                        server_url,
                        &writer,
                        &shutdown,
                        &mut maybe_ws,
                    )
                    .await
                    {
                        eprintln!(
                            "{}",
                            format!("处理协议行出错：{}", e).red().bold()
                        );
                        if let Some(ws) = maybe_ws.as_mut() {
                            if let Err(e2) = ws.close().await {
                                eprintln!(
                                    "{}",
                                    format!("协议处理错误后关闭 websocket 失败：{}", e2)
                                        .red()
                                        .bold()
                                );
                            }
                        }
                        shutdown.notify_waiters();
                        break;
                    }
                }
            }
            _ = shutdown.notified() => {
                println!(
                    "{}",
                    "由于 websocket 断开，停止串口读取循环。"
                        .yellow()
                        .bold()
                );
                break;
            }
        }
    }

    Ok(())
}

fn spawn_heartbeat_task(
    writer: &Arc<Mutex<tokio::io::WriteHalf<SerialStream>>>,
    shutdown: &Arc<Notify>,
) {
    let writer = Arc::clone(writer);
    let shutdown = Arc::clone(shutdown);
    tokio::spawn(async move {
        const HEARTBEAT_INTERVAL_SECS: u64 = 5;
        loop {
            tokio::select! {
                _ = shutdown.notified() => {
                    println!(
                        "{}",
                        "由于收到关闭信号，停止心跳任务。"
                            .yellow()
                            .bold()
                    );
                    break;
                }
                _ = tokio::time::sleep(Duration::from_secs(HEARTBEAT_INTERVAL_SECS)) => {
                    let mut writer_guard = writer.lock().await;
                    if let Err(e) = writer_guard.write_all(b"referee heartbeat\n").await {
                        eprintln!("通过串口发送心跳失败：{}", e);
                        break;
                    }
                }
            }
        }
    });
}

async fn handle_protocol_line(
    short_sn: &str,
    hex_str: &str,
    server_url: &str,
    writer: &Arc<Mutex<tokio::io::WriteHalf<SerialStream>>>,
    shutdown: &Arc<Notify>,
    maybe_ws: &mut Option<WebSocketSink>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let bytes = match hex_to_bytes(hex_str) {
        Ok(bytes) => bytes,
        Err(e) => {
            eprintln!(
                "{}",
                format!("解析十六进制载荷 '{}' 失败：{}", hex_str, e)
                    .red()
                    .bold()
            );
            return Ok(());
        }
    };

    if bytes.len() < 4 {
        eprintln!(
            "{}",
            format!("协议载荷过短（{} 字节），已忽略。", bytes.len())
                .yellow()
                .bold()
        );
        return Ok(());
    }

    ensure_websocket_connected(
        short_sn,
        server_url,
        writer,
        shutdown,
        maybe_ws,
    )
    .await?;

    if let Some(ws) = maybe_ws.as_mut() {
        // 仅将协议数据转发至 websocket，不再逐条打印日志避免过于冗长
        ws.send(Message::Binary(bytes.into())).await?;
    }

    Ok(())
}

async fn ensure_websocket_connected(
    short_sn: &str,
    server_url: &str,
    writer: &Arc<Mutex<tokio::io::WriteHalf<SerialStream>>>,
    shutdown: &Arc<Notify>,
    maybe_ws: &mut Option<WebSocketSink>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    if maybe_ws.is_some() {
        return Ok(());
    }

    match connect_device_ws(short_sn, server_url).await {
        Ok(ws) => {
            let (ws_tx, ws_rx) = ws.split();
            *maybe_ws = Some(ws_tx);
            spawn_receive_websocket(ws_rx, writer, shutdown);
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn spawn_receive_websocket(
    mut ws_rx: WebSocketRecvStream,
    writer: &Arc<Mutex<tokio::io::WriteHalf<SerialStream>>>,
    shutdown: &Arc<Notify>,
) {
    let writer_for_ws = Arc::clone(writer);
    let shutdown = Arc::clone(shutdown);
    tokio::spawn(async move {
        while let Some(msg_result) = ws_rx.next().await {
            match msg_result {
                Ok(Message::Binary(data)) => {
                    let hex: String = data.iter().map(|b| format!("{:02X}", b)).collect();
                    let line = format!("referee send {}\n", hex);
                    let mut writer_guard = writer_for_ws.lock().await;
                    if let Err(e) = writer_guard.write_all(line.as_bytes()).await {
                        eprintln!(
                            "{}",
                            format!("转发websocket二进制数据到串口失败：{}", e)
                                .red()
                                .bold()
                        );
                        shutdown.notify_waiters();
                        break;
                    }
                }
                Ok(Message::Close(frame)) => {
                    println!(
                        "{} {:?}",
                        "websocket连接被服务器关闭："
                            .yellow()
                            .bold(),
                        frame
                    );
                    shutdown.notify_waiters();
                    break;
                }
                Ok(_) => {
                    // Ignore non-binary messages.
                }
                Err(e) => {
                    eprintln!(
                        "{}",
                        format!("接收websocket消息出错：{}", e)
                            .red()
                            .bold()
                    );
                    shutdown.notify_waiters();
                    break;
                }
            }
        }
    });
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

    println!("{}", "正在搜索裁判系统服务器...".bold().cyan());

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
            let url = format!("ws://{}:{}", host, socket_addr.port());
            return Ok(url);
        }

        if let Some(addr) = response.ip_addr() {
            let host = match addr {
                IpAddr::V4(addr) => addr.to_string(),
                IpAddr::V6(addr) => format!("[{}]", addr),
            };
            let url = format!("ws://{}:3000", host);
            return Ok(url);
        }
    }
    Err("未找到裁判系统服务器".into())
}

async fn connect_device_ws(
    short_sn: &str,
    server_url: &str,
) -> Result<WebSocketStream, Box<dyn std::error::Error + Send + Sync>> {

    let url_str = format!("{}/ws/devices/{}", server_url, short_sn);
    println!(
        "{} {}",
        "正在建立websocket连接:".bold().cyan(),
        url_str.cyan()
    );

    let (mut socket, response) = connect_async(&url_str).await?;
    let _ = response;

    println!(
        "{} {}",
        "websocket已连接，设备短SN:".bold().green(),
        short_sn.cyan()
    );

    // Optionally, we could perform a ping here to ensure connection is alive.
    if let Err(e) = socket.send(Message::Ping(vec![].into())).await {
        eprintln!(
            "{}",
            format!("websocket建立后发送ping失败：{}", e)
                .yellow()
                .bold()
        );
    }

    Ok(socket)
}
