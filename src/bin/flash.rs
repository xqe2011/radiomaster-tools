use colored::*;
use espflash::connection::{Connection, ResetAfterOperation, ResetBeforeOperation};
use espflash::flasher::Flasher;
use espflash::target::ProgressCallbacks;
use indicatif::{ProgressBar, ProgressStyle};
use log::{warn, Level};
use serialport::{
    available_ports, DataBits, FlowControl, Parity, SerialPortInfo, SerialPortType, StopBits,
    UsbPortInfo,
};
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

// Layout based on ESP-IDF's esp_app_desc_t definition.
// See: app_image_format documentation in ESP-IDF.
#[derive(Debug)]
struct AppDescription {
    secure_version: u32,
    version: String,
    project_name: String,
    time: String,
    date: String,
    idf_ver: String,
    app_elf_sha256: [u8; 32],
}

#[derive(Debug, Copy, Clone)]
enum FirmwareType {
    AppImage,
    FullImage,
}

const EXPECTED_MAGIC_WORD: u32 = 0xABCD_5432;

// Offsets for detecting and reading esp_app_desc_t in different image types.
// 1. App-Image   : magic at 0x20-0x23 from start of file.
// 2. Full-Image  : magic at 0x10000 + 0x20-0x23.
const APP_IMAGE_DESC_OFFSET: usize = 0x20;
const FULL_IMAGE_APP_DESC_OFFSET: usize = 0x10000 + 0x20;

fn main() -> io::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format(|buf, record| {
            let level = match record.level() {
                Level::Error => "ERROR".red().bold(),
                Level::Warn => "WARN".yellow().bold(),
                Level::Info => "INFO".cyan().bold(),
                Level::Debug => "DEBUG".blue().bold(),
                Level::Trace => "TRACE".white().bold(),
            };
            writeln!(buf, "{} {} {}", buf.timestamp(), level, record.args())
        })
        .init();

    // Ask user for firmware file path instead of using a hard-coded default.
    print!("{}", "请输入固件文件路径：".bold());
    io::stdout().flush()?;

    let mut firmware_path_input = String::new();
    io::stdin().read_line(&mut firmware_path_input)?;

    // Trim whitespace and optional surrounding quotes (both ' and ").
    let firmware_path_input = firmware_path_input
        .trim()
        .trim_matches(|c| c == '"' || c == '\'');

    if firmware_path_input.is_empty() {
        eprintln!(
            "{}",
            "错误：固件路径不能为空。".red().bold()
        );
        std::process::exit(1);
    }

    let firmware_path = Path::new(firmware_path_input);
    if !firmware_path.exists() {
        eprintln!(
            "{}",
            format!(
                "固件文件 '{}' 未找到。",
                firmware_path.display()
            )
            .red()
            .bold()
        );
        std::process::exit(1);
    }

    // Read firmware into memory and print some basic info.
    let mut firmware_file = File::open(&firmware_path)?;
    let mut firmware_data = Vec::new();
    firmware_file.read_to_end(&mut firmware_data)?;

    // Firmware summary banner (start).
    println!(
        "{}",
        "============================== 固件摘要 =============================="
            .bold()
            .cyan()
    );

    println!(
        "{} {}",
        "固件文件    :".bold().green(),
        format!("{}", firmware_path.display()).cyan()
    );
    println!(
        "{} {} 字节",
        "固件大小    :".bold().green(),
        firmware_data.len().to_string().cyan()
    );
    println!(
      "{} {}",
      "固件预览    :"
          .bold()
          .green(),
      to_hex_preview(&firmware_data, 16).bright_black()
    );

    // Parse esp_image_header_t information.
    if let Some(header) = parse_image_header(&firmware_data) {
        println!(
            "{} {}",
            "芯片类型    :".bold().green(),
            header.chip_id.cyan()
        );
        println!(
            "{} {}",
            "SPI大小     :".bold().green(),
            header.spi_size.cyan()
        );
    } else {
        warn!("无法从固件镜像中解析 esp_image_header_t。");
    }

    // Try to parse and print esp_app_desc_t information (esp_app_desc_t),
    // and determine firmware type for flashing.
    let (app_desc, firmware_type) = match parse_app_description(&firmware_data) {
        Some(v) => v,
        None => {
            eprintln!(
                "{}",
                "错误：无法从固件镜像中找到有效的 esp_app_desc_t。"
                    .red()
                    .bold()
            );
            std::process::exit(1);
        }
    };

    let firmware_type_str = match firmware_type {
        FirmwareType::AppImage => "应用固件",
        FirmwareType::FullImage => "完整固件",
    };

    println!(
        "{} {}",
        "固件类型    :".bold().green(),
        firmware_type_str.cyan()
    );

    println!(
        "{} {}",
        "安全版本    :".bold().green(),
        format!("{} (防回滚)", app_desc.secure_version).cyan()
    );
    println!(
        "{} {}",
        "版本        :".bold().green(),
        app_desc.version.cyan()
    );
    println!(
        "{} {}",
        "项目名称    :".bold().green(),
        app_desc.project_name.cyan()
    );

    let build_time = format_build_datetime(&app_desc.date, &app_desc.time)
        .unwrap_or_else(|| format!("{} {}", app_desc.date, app_desc.time));
    println!(
        "{} {}",
        "构建时间    :".bold().green(),
        build_time.cyan()
    );

    println!(
        "{} {}",
        "IDF版本     :".bold().green(),
        app_desc.idf_ver.cyan()
    );
    let sha256_hex: String = app_desc
        .app_elf_sha256
        .iter()
        .map(|b| format!("{:02x}", b))
        .collect();
    println!(
        "{} {}",
        "应用SHA256  :".bold().green(),
        sha256_hex.cyan()
    );

    // Firmware summary banner (end).
    println!(
        "{}",
        "============================== 固件摘要 =============================="
            .bold()
            .cyan()
    );

    // Search for USB ports after firmware info is displayed.
    let port_info = select_serial_port()?;
    let port_name = port_info.port_name.clone();

    // Ask user to confirm before flashing.
    println!();
    println!(
        "即将将 '{}' 烧录到端口 '{}' 上的设备。",
        firmware_path.display(),
        port_name
    );
    println!(
        "{}",
        "按 'Y' 然后回车确认，其他键取消。"
            .bold()
            .yellow()
    );
    print!("{}", "确认烧录？[Y/n]: ".bold());
    io::stdout().flush()?;

    let mut input = String::new();
    io::stdin().read_line(&mut input)?;
    if !input.trim().eq_ignore_ascii_case("y") {
        println!("{}", "烧录已取消。".yellow());
        return Ok(());
    }

    // Open the selected serial port as a native port (TTYPort/COMPort).
    let serial_port = serialport::new(&port_name, 115_200)
        .data_bits(DataBits::Eight)
        .flow_control(FlowControl::None)
        .parity(Parity::None)
        .stop_bits(StopBits::One)
        .timeout(Duration::from_millis(3000))
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

    // Build espflash connection and flasher.
    let connection = Connection::new(
        *Box::new(serial_port),
        usb_info,
        ResetAfterOperation::NoReset,
        ResetBeforeOperation::DefaultReset,
        115_200,
    );

    let mut flasher = Flasher::connect(connection, true, true, false, None, Some(115_200))
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    // Perform flashing using espflash APIs.
    // If firmware type is App-Image, flash at 0x10000; otherwise (Full-Image) flash at 0x0.
    let flash_addr: u32 = match firmware_type {
        FirmwareType::AppImage => 0x10000,
        FirmwareType::FullImage => 0x0,
    };

    println!(
        "{} 0x{:08X}",
        "正在烧录固件到地址:".bold().cyan(),
        flash_addr
    );

    let mut progress = ProgressBarCallback::default();
    flasher
        .write_bin_to_flash(flash_addr, &firmware_data, &mut progress)
        .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;

    println!("{}", "固件烧录成功。".bold().green());

    // Manually reset the device after successful flashing
    println!("{}", "正在重启设备...".bold().cyan());
    // Get connection back from flasher to perform manual reset
    let mut connection = flasher.into_connection();
    connection.reset().map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
    std::thread::sleep(Duration::from_millis(500));
    println!("{}", "设备已重启。".bold().green());

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

fn to_hex_preview(data: &[u8], max_len: usize) -> String {
    let take_len = data.len().min(max_len);
    let mut s = String::new();
    for (i, b) in data.iter().take(take_len).enumerate() {
        if i > 0 {
            s.push(' ');
        }
        s.push_str(&format!("{:02X}", b));
    }
    if data.len() > take_len {
        s.push_str(" ...");
    }
    s
}

// Helper to read u32 little-endian from a 4-byte slice.
fn read_u32_le(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

fn parse_app_description(image: &[u8]) -> Option<(AppDescription, FirmwareType)> {
    // esp_app_desc_t layout (C):
    // uint32_t magic_word;
    // uint32_t secure_version;
    // uint32_t reserved1[2];
    // uint8_t  version[32];
    // uint8_t  project_name[32];
    // uint8_t  time[16];
    // uint8_t  date[16];
    // uint8_t  idf_ver[32];
    // uint8_t  app_elf_sha256[32];
    // uint32_t reserved2[8];
    const APP_DESC_SIZE: usize = 208;

    // 1. App-Image: if 0x20-0x23 == EXPECTED_MAGIC_WORD, esp_app_desc_t is at APP_IMAGE_DESC_OFFSET.
    // 2. Full-Image: otherwise, check 0x10000 + 0x20-0x23; if matches, esp_app_desc_t is there.
    let is_app_image = if image.len() >= APP_IMAGE_DESC_OFFSET + 4 {
        let magic = read_u32_le(&image[APP_IMAGE_DESC_OFFSET..APP_IMAGE_DESC_OFFSET + 4]);
        magic == EXPECTED_MAGIC_WORD
    } else {
        false
    };

    let is_full_image = if !is_app_image && image.len() >= FULL_IMAGE_APP_DESC_OFFSET + 4 {
        let magic = read_u32_le(
            &image[FULL_IMAGE_APP_DESC_OFFSET..FULL_IMAGE_APP_DESC_OFFSET + 4],
        );
        magic == EXPECTED_MAGIC_WORD
    } else {
        false
    };

    let (base_offset, firmware_type) = if is_app_image {
        (APP_IMAGE_DESC_OFFSET, FirmwareType::AppImage)
    } else if is_full_image {
        (FULL_IMAGE_APP_DESC_OFFSET, FirmwareType::FullImage)
    } else {
        return None;
    };

    if image.len() < base_offset + APP_DESC_SIZE {
        return None;
    }

    let desc_bytes = &image[base_offset..base_offset + APP_DESC_SIZE];

    let secure_version = read_u32_le(&desc_bytes[4..8]);

    // Skip reserved1[2] (8 bytes).
    let mut offset = 16usize;

    let version_raw = &desc_bytes[offset..offset + 32];
    offset += 32;
    let project_name_raw = &desc_bytes[offset..offset + 32];
    offset += 32;
    let time_raw = &desc_bytes[offset..offset + 16];
    offset += 16;
    let date_raw = &desc_bytes[offset..offset + 16];
    offset += 16;
    let idf_ver_raw = &desc_bytes[offset..offset + 32];
    offset += 32;
    let app_elf_sha256_raw = &desc_bytes[offset..offset + 32];

    let mut app_elf_sha256 = [0u8; 32];
    app_elf_sha256.copy_from_slice(app_elf_sha256_raw);

    Some((
        AppDescription {
            secure_version,
            version: read_cstring(version_raw),
            project_name: read_cstring(project_name_raw),
            time: read_cstring(time_raw),
            date: read_cstring(date_raw),
            idf_ver: read_cstring(idf_ver_raw),
            app_elf_sha256,
        },
        firmware_type,
    ))
}

fn read_cstring(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    // Fields are ASCII; fall back to lossy UTF-8 just in case.
    String::from_utf8_lossy(&buf[..end]).to_string()
}

#[derive(Debug)]
struct ImageHeader {
    chip_id: String,
    spi_size: String,
}

fn parse_image_header(image: &[u8]) -> Option<ImageHeader> {
    const HEADER_SIZE: usize = 24;
    if image.len() < HEADER_SIZE {
        return None;
    }

    // Layout per esp_image_header_t from ESP-IDF.
    // Structure (packed):
    // offset 0: magic (uint8_t)
    // offset 1: segment_count (uint8_t)
    // offset 2: spi_mode (uint8_t)
    // offset 3: spi_speed (uint8_t)
    // offset 4: spi_size (uint8_t) - esp_image_flash_size_t
    // offset 5-8: entry_addr (uint32_t)
    // offset 9: wp_pin (uint8_t)
    // offset 10-12: spi_pin_drv[3] (uint8_t[3])
    // offset 12-13: chip_id (uint16_t) - esp_chip_id_t
    let magic = image[0];
    if magic != 0xE9 {
        return None;
    }

    // spi_size is at offset 4 (esp_image_flash_size_t enum)
    let spi_size_code = image[3] >> 4;
    let spi_size = match spi_size_code {
        0 => "1 MB".to_string(),
        1 => "2 MB".to_string(),
        2 => "4 MB".to_string(),
        3 => "8 MB".to_string(),
        4 => "16 MB".to_string(),
        code => format!("未知 (代码 {code})"),
    };

    // chip_id is at offset 12-13 (uint16_t, esp_chip_id_t enum)
    // Read as little-endian uint16_t
    let chip_id_raw = u16::from_le_bytes([image[12], image[13]]);
    let chip_id = chip_id_from_header(chip_id_raw);

    Some(ImageHeader {
        chip_id,
        spi_size,
    })
}

fn chip_id_from_header(chip: u16) -> String {
    // Match esp_chip_id_t enum values from ESP-IDF
    match chip {
        0x0000 => "ESP32".to_string(),
        0x0002 => "ESP32-S2".to_string(),
        0x0005 => "ESP32-C3".to_string(),
        0x0009 => "ESP32-S3".to_string(),
        0x000A => "ESP32-H2".to_string(),
        0x000C => "ESP32-C2".to_string(),
        0x000D => "ESP32-C6".to_string(),
        0x0012 => "ESP32-P4".to_string(),
        0xFFFF => "无效".to_string(),
        other => format!("未知 (0x{other:04X})"),
    }
}

fn format_build_datetime(date_str: &str, time_str: &str) -> Option<String> {
    // ESP-IDF stores date as "Mmm DD YYYY" and time as "HH:MM:SS".
    let mut parts = date_str.split_whitespace();
    let month_str = parts.next()?;
    let day_str = parts.next()?;
    let year_str = parts.next()?;

    let month = match month_str {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };

    let day: u32 = day_str.parse().ok()?;
    let year: i32 = year_str.parse().ok()?;

    let time_clean = time_str.trim();
    if !time_clean.contains(':') {
        return None;
    }

    Some(format!("{:04}-{:02}-{:02} {}", year, month, day, time_clean))
}

#[derive(Default)]
struct ProgressBarCallback {
    bar: Option<ProgressBar>,
}

impl ProgressCallbacks for ProgressBarCallback {
    fn init(&mut self, _addr: u32, total: usize) {
        let bar = ProgressBar::new(total as u64);
        bar.set_style(
            ProgressStyle::with_template("{bar:40.cyan/blue} {pos:>8}/{len:>8} ({percent:>3}%) [{eta}]")
                .unwrap()
                .progress_chars("##-"),
        );
        self.bar = Some(bar);
    }

    fn update(&mut self, current: usize) {
        if let Some(bar) = self.bar.as_ref() {
            bar.set_position(current as u64);
        }
    }

    fn verifying(&mut self) {
        if let Some(bar) = self.bar.as_ref() {
            bar.set_message("正在验证...");
        }
    }

    fn finish(&mut self, skipped: bool) {
        if let Some(bar) = self.bar.take() {
            if skipped {
                bar.finish_with_message("已跳过");
            } else {
                bar.finish_with_message("完成");
            }
        }
    }
}
