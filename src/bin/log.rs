use colored::*;
use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use serialport::{
    available_ports, DataBits, FlowControl, Parity, SerialPortInfo, SerialPortType, StopBits,
    UsbPortInfo,
};
use std::fs::File;
use std::io::{self, BufRead, BufReader, Write};
use std::time::Duration;

fn main() -> io::Result<()> {
    // Enable ANSI color support on Windows consoles (no-op on other platforms).
    #[cfg(windows)]
    {
        let _ = colored::control::set_virtual_terminal(true);
    }

    // Select serial port (same logic as flash.rs)
    let port_info = select_serial_port()?;
    let port_name = port_info.port_name.clone();

    // Get serial number for filename
    let serial_number = match &port_info.port_type {
        SerialPortType::UsbPort(info) => info
            .serial_number
            .as_deref()
            .unwrap_or("UNKNOWN")
            .to_string(),
        _ => "UNKNOWN".to_string(),
    };

    // Generate log filename with timestamp
    let now = chrono::Local::now();
    let log_filename = format!(
        "logs_{}_{}.txt",
        serial_number,
        now.format("%Y-%m-%d-%H-%M-%S")
    );

    println!(
        "{} {}",
        "日志文件:".bold().green(),
        log_filename.cyan()
    );
    println!(
        "{} {}",
        "串口:".bold().green(),
        port_name.cyan()
    );

    // Open log file
    let mut log_file = File::create(&log_filename)?;

    // Open serial port
    let serial_port = serialport::new(&port_name, 115_200)
        .data_bits(DataBits::Eight)
        .flow_control(FlowControl::None)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .timeout(Duration::from_millis(100))  // Shorter timeout for faster reading
        .open_native()
        .map_err(|e| {
            eprintln!(
                "{}",
                format!("打开串口 '{}' 失败：{}", port_name, e)
                    .red()
                    .bold()
            );
            io::Error::new(io::ErrorKind::Other, "failed to open serial port")
        })?;

    // Convert serial port info into UsbPortInfo required by espflash Connection.
    let usb_info = match &port_info.port_type {
        SerialPortType::UsbPort(info) => info.clone(),
        SerialPortType::PciPort | SerialPortType::Unknown => UsbPortInfo {
            vid: 0,
            pid: 0,
            serial_number: None,
            manufacturer: None,
            product: None,
        },
        SerialPortType::BluetoothPort => {
            return Err(io::Error::new(
                io::ErrorKind::Other,
                format!("不支持的串口类型：{:?}", port_info.port_type),
            ));
        }
    };

    // Build espflash connection and reset the device
    println!("{}", "正在重启设备...".bold().cyan());
    let mut connection = Connection::new(
        *Box::new(serial_port),
        usb_info,
        ResetAfterOperation::NoReset,
        ResetBeforeOperation::DefaultReset,
        115_200,
    );
    connection.reset().map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    
    // Extract the serial port back from the connection
    let port = connection.into_serial();
    
    println!("{}", "开始记录日志 (按 Ctrl+C 退出)...".bold().green());
    println!();

    // Use a larger buffer for better performance (64KB buffer)
    let mut reader = BufReader::with_capacity(64 * 1024, port);
    let mut line = String::new();

    loop {
        line.clear();
        match reader.read_line(&mut line) {
            Ok(0) => {
                // EOF - port closed
                println!("{}", "串口已关闭。".yellow().bold());
                break;
            }
            Ok(_) => {
                // Process the line
                let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
                if trimmed.is_empty() {
                    continue;
                }

                // Color based on first character
                let colored_line = if trimmed.starts_with('I') {
                    trimmed.green().to_string()
                } else if trimmed.starts_with('W') {
                    trimmed.yellow().to_string()
                } else if trimmed.starts_with('E') {
                    trimmed.red().to_string()
                } else {
                    trimmed.white().to_string()
                };

                // Print to screen with color
                println!("{}", colored_line);

                // Write raw line (without color codes) to file
                log_file.write_all(line.as_bytes())?;
            }
            Err(e) if e.kind() == io::ErrorKind::TimedOut => {
                // Timeout is expected, continue reading
                continue;
            }
            Err(e) => {
                eprintln!("{}", format!("读取串口错误：{}", e).red().bold());
                break;
            }
        }
    }

    // Final flush to ensure all data is written
    log_file.flush()?;

    println!();
    println!(
        "{} {}",
        "日志已保存到:".bold().green(),
        log_filename.cyan()
    );

    Ok(())
}

fn select_serial_port() -> io::Result<SerialPortInfo> {
    loop {
        let ports = available_ports().map_err(|e| {
            eprintln!("{}", format!("列出串口失败：{e}").red().bold());
            io::Error::new(io::ErrorKind::Other, "failed to list serial ports")
        })?;

        let filtered: Vec<SerialPortInfo> = ports
            .into_iter()
            .filter_map(|port| match &port.port_type {
                SerialPortType::UsbPort(_) => Some(port),
                _ => None,
            })
            .collect();

        if filtered.is_empty() {
            println!(
                "{}",
                "未找到 USB 串口，正在继续搜索..."
                    .yellow()
                    .bold()
            );
            std::thread::sleep(Duration::from_secs(2));
            continue;
        }

        if filtered.len() == 1 {
            let only = &filtered[0];
            if let SerialPortType::UsbPort(info) = &only.port_type {
                println!(
                    "{} {} (VID: 0x{:04X}, PID: 0x{:04X})",
                    "使用唯一可用的串口：".bold().green(),
                    only.port_name,
                    info.vid,
                    info.pid
                );
            } else {
                println!(
                    "{} {}",
                    "使用唯一可用的串口：".bold().green(),
                    only.port_name
                );
            }
            return Ok(only.clone());
        }

        println!(
            "{}",
            "可用 USB 串口:".bold().cyan()
        );
        for (i, port) in filtered.iter().enumerate() {
            if let SerialPortType::UsbPort(info) = &port.port_type {
                println!(
                    "{i}: {} (VID: 0x{:04X}, PID: 0x{:04X}, SN: {})",
                    port.port_name,
                    info.vid,
                    info.pid,
                    info
                        .serial_number
                        .as_deref()
                        .unwrap_or("未知")
                );
            } else {
                println!("{i}: {}", port.port_name);
            }
        }

        loop {
            print!("{}", "请选择端口编号：".bold());
            io::stdout().flush()?;

            let mut input = String::new();
            io::stdin().read_line(&mut input)?;

            let trimmed = input.trim();
            match trimmed.parse::<usize>() {
                Ok(idx) if idx < filtered.len() => {
                    let chosen = filtered[idx].clone();
                    println!("{} {}", "已选择端口：".bold().green(), chosen.port_name);
                    return Ok(chosen);
                }
                _ => {
                    println!(
                        "{}",
                        format!(
                            "无效选择 '{trimmed}'。请输入 0 到 {} 之间的数字。",
                            filtered.len() - 1
                        )
                        .yellow()
                    );
                }
            }
        }
    }
}
