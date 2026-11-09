use std::fs::File;
use std::path::PathBuf;
use std::process::exit;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use std::io::{stdout, Write};

use clap::{Parser, Subcommand, ValueEnum};

use clap_num::maybe_hex;
use derive_more::{Display, From};
use jiff::Zoned;
use rusb::{Device, DeviceHandle, GlobalContext};
use sirin_shared::mode::SirinMode;
use sirin_shared::packet::{ByteArrayStrError, InPacket, Log, LogEntry, OutPacket, MAX_OUT_PACKET_SIZE};
use sirin_shared::config::{CallsignBuf, NicknameBuf};
use sirin_shared::song::magic::MagicU8;
use sirin_shared::song::{FromSong, FromSongError, SongSize, ToSong, ToSongError};
use sirin_shared::time::AbsoluteTimeReference;
use sirin_shared::usb::{USB_EP_IN_ADDR, USB_EP_OUT_ADDR, USB_PID, USB_VID};
use sirin_shared::packet::ByteArrayStr;

#[derive(Parser)]
#[command(version, about)]
struct Args {
    #[command(subcommand)]
    command: Subcommands
}

#[derive(Subcommand)]
enum Subcommands {
    #[command(about = "List connected Sirins")]
    Ls,
    #[command(about = "Erase everything")]
    Erase,
    #[command(about = "Reboot Sirin")]
    Reboot,
    #[command(about = "See the outgoing Sirin packets")]
    Tail,

    #[command(about = "View and set configuration")]
    Config {
        #[arg(long)]
        nickname: Option<String>,

        #[arg(long)]
        callsign: Option<String>,

        #[arg(long, value_parser=maybe_hex::<u16>)]
        id: Option<u16>
    },
    #[command(about = "View and set the mode, e.g. flight/standby")]
    Mode {
        mode: Option<Mode>
    },
    #[command(subcommand, about = "List and export flights")]
    Flights(FlightSubcommands),
    #[command(subcommand, about = "Output NMEA strings")]
    Gps,
}

#[derive(Subcommand)]
enum FlightSubcommands {
    #[command(about = "List all saved flights")]
    Ls,

    #[command(about = "Export data from a flight")]
    Export {
        index: u16,
        
        #[arg(long)]
        csv: PathBuf
    }
}

#[derive(ValueEnum, Clone)]
enum Mode {
    Standby,
    Flight
}

fn main() {
    let args = Args::parse();

    match run(args) {
        Ok(_) => {},
        Err(err) => {
            println!("{}", err);
            exit(1)
        }
    }
}

fn run(args: Args) -> Result<(), Error> {
    let _ = set_time();

    match args.command {
        Subcommands::Ls => {
            SirinDev::list();
        }
        Subcommands::Erase => {
            let sirin = SirinDev::connect()?.open()?;
            sirin.send_packet(&InPacket::EraseFlash(MagicU8::<0xA8>))?;

            println!("Erasing. This may take some time.");

            for _ in sirin.receive_packets() {}

            println!("Done. Please unplug and plug back in Sirin.");
        }
        Subcommands::Reboot => {
            let sirin = SirinDev::connect()?.open()?;
            sirin.send_packet(&InPacket::Reboot)?;
        }
        Subcommands::Tail => {
            let sirin = SirinDev::connect()?.open()?;
            sirin.send_packet(&InPacket::Tail(true))?;
            for packet in sirin.receive_packets() {
                match packet {
                    Ok(packet) => println!("{:?}", packet),
                    Err(Error::NoSirin) => return Err(Error::NoSirin),
                    Err(err) => println!("Error: {}", err)
                }
            }
        }
        Subcommands::Config { nickname, callsign, id } => {
            let is_changing_config = nickname.is_some() || callsign.is_some() || id.is_some();

            let sirin = SirinDev::connect()?.open()?;
            sirin.send_packet(&InPacket::QueryConfig)?;
            
            let mut config = loop {
                let packet = sirin.receive_packet()?;
                let OutPacket::Config(config) = packet else {
                    continue;
                };

                break config;
            };

            if is_changing_config {
                println!("Old config:");
            }
            println!("{}", config);

            if is_changing_config {
                if let Some(nickname) = nickname {
                    config.nickname = NicknameBuf::from_str(&nickname)?;
                }

                if let Some(callsign) = callsign {
                    config.callsign = CallsignBuf::from_str(&callsign)?;
                }

                if let Some(id) = id {
                    config.id = id;
                }

                println!("\nNew config:");
                println!("{}", config);

                sirin.send_packet(&InPacket::SetConfig(config))?;
            }
        }
        Subcommands::Mode { mode } => {
            let sirin = SirinDev::connect()?.open()?;

            if let Some(mode) = mode {
                let mode = match mode {
                    Mode::Standby => SirinMode::Standby,
                    Mode::Flight => SirinMode::Flight
                };

                sirin.send_packet(&InPacket::SetMode(mode))?;

                println!("Set the mode to {}.", mode)
            } else {
                sirin.send_packet(&InPacket::QueryMode)?;

                let mode = loop {
                    let packet = sirin.receive_packet()?;
                    let OutPacket::Mode(mode) = packet else {
                        continue;
                    };

                    break mode;
                };

                println!("{}", mode);
            }
        }
        Subcommands::Flights(subcommand) => {
            match subcommand {
                FlightSubcommands::Ls => {
                    let sirin = SirinDev::connect()?.open()?;
                    
                    sirin.send_packet(&InPacket::QueryFlights)?;

                    let mut empty = true;
                    for packet in sirin.receive_packets() {
                        match packet {
                            Ok(OutPacket::FlightHeader(header)) => {
                                if empty {
                                    println!(" ID  Date");
                                    println!("===========================")
                                }

                                empty = false;
                                if let Some(time) = header.data.time_reference.0 {
                                    let time = UNIX_EPOCH + Duration::from_millis(time.ms_since_epoch);

                                    if let Ok(zoned) = Zoned::try_from(time) {
                                        println!("{:>3}  {}", header.index, zoned.strftime("%A, %B %d, %Y at %I:%M%p %Q"));
                                    } else {
                                        println!("{:>3}  Unknown time", header.index);
                                    }
                                } else {
                                    println!("{:>3}  Unknown time", header.index);
                                }
                            }
                            Err(Error::NoSirin) => return Err(Error::NoSirin),
                            Err(err) => println!("Error: {}", err),
                            _ => {}
                        }
                    }

                    if empty {
                        println!("No flights found.");
                    }
                },
                FlightSubcommands::Export { index, csv } => {
                    let sirin = SirinDev::connect()?.open()?;
                    println!("Writing flight data to {}", csv.to_string_lossy());
                    let mut file = File::create(csv)?;
                    file.write_all(b"time (ms), flight_mode, data_altitude (m), state_altitude (m), apogee(m), GPS_lat (deg), GPS_lon (deg), GPS_ecef_x (m), GPS_ecef_y (m), GPS_ecef_z (m), pos_x (deg), pos_y (deg), pos_z (deg), vel_x (m/s), vel_y (m/s), vel_z (m/s), pressure (Pa),temp (C),accel_x (\xB5G),accel_y (\xB5G),accel_z (\xB5G),ang_vel_x (\xB5\xB0/s),ang_vel_y (\xB5\xB0/s),ang_vel_z (\xB5\xB0/s),high_g_x (\xB5G),high_g_y (\xB5G),high_g_z (\xB5G),mag_x (G),mag_y (G),mag_z (G)\n")?;
                    sirin.send_packet(&InPacket::ReadFlight(index))?;
                    for packet in sirin.receive_packets() {
                        match packet.unwrap() {
                            OutPacket::LogEntry(entry) => {
                                    #[allow(irrefutable_let_patterns)]
                                    if let Log::DataState(datastate) = entry.log {
                                        let imu_accel = datastate.data.imu.accel.unwrap();
                                        let ang_vel = datastate.data.imu.angular_vel.unwrap();
                                        let mag = datastate.data.magnetometer.mag.unwrap();
                                        //println!("SirinDataState entry detected!");
                                        writeln!(
                                            file,
                                            "{},{},{},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?},{:?}",
                                            datastate.data.time.value,
                                            datastate.mode,
                                            datastate.data.altitude,
                                            datastate.altitude,
                                            datastate.apogee,
                                            datastate.gps_fix.lat,
                                            datastate.gps_fix.lon,
                                            datastate.gps_fix.pos.x.value,
                                            datastate.gps_fix.pos.y.value,
                                            datastate.gps_fix.pos.z.value,
                                            datastate.pos.x.value,
                                            datastate.pos.y.value,
                                            datastate.pos.z.value,
                                            datastate.vel.x.value,
                                            datastate.vel.y.value,
                                            datastate.vel.z.value,
                                            datastate.data.baro.pressure.unwrap().value,
                                            datastate.data.baro.temperature.unwrap().value,
                                            imu_accel.x,
                                            imu_accel.y,
                                            imu_accel.z,
                                            ang_vel.x,
                                            ang_vel.y,
                                            ang_vel.z,
                                            mag.x,
                                            mag.y,
                                            mag.z
                                        )?;}
                                    }
                            OutPacket::DeployedApoAt(time) => {
                                println!("Apo deployed at {} milliseconds", time);
                            }
                            OutPacket::DeployedMainAt(time) => {
                                println!("Main deployed at {} milliseconds", time);
                            }
                            _ => {}
                                }

                            }
                        }
                    }
                    println!("Done.");
                },
        //TODO: Fix this
        /*Subcommands::Gps => {
            let sirin = SirinDev::connect()?.open()?;

            loop {
                let Ok(OutPacket::LogEntry(LogEntry { log: Log::GpsNmea(msg), .. })) = sirin.receive_packet() else {
                    continue;
                };

                let _ = stdout().write(&msg);
            }
        }*/
        Subcommands::Gps =>{}
    }

    Ok(())
}

fn set_time() -> Result<(), Error> {
    let sirin = SirinDev::connect()?.open()?;

    sirin.send_packet(&InPacket::SetTime(AbsoluteTimeReference {
        ms_since_epoch: SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_millis() as u64
    }))?;

    Ok(())
}

pub struct SirinDev {
    device: Device<GlobalContext>
}

#[derive(From, Display, Debug)]
pub enum Error {
    #[display("No Sirin found.")]
    NoSirin,
    #[display("There was an error with the USB: {_0}")]
    UsbError(rusb::Error),
    #[from]
    #[display("There was an error decoding the packet: {_0:?}")]
    FromSongError(FromSongError),
    #[from]
    #[display("There was an error encoding the packet: {_0:?}")]
    ToSongError(ToSongError),
    #[from]
    #[display("There was an error with a byte array string: {_0:?}")]
    ByteArrayStr(ByteArrayStrError),
    #[from]
    IoError(std::io::Error)
}

impl From<rusb::Error> for Error {
    fn from(value: rusb::Error) -> Self {
        match value {
            rusb::Error::NoDevice => Self::NoSirin,
            err => Self::UsbError(err)
        }
    }
}

impl SirinDev {
    fn list() {
        let mut count = 0;
        for device in rusb::devices().unwrap().iter() {
            let device_desc = device.device_descriptor().unwrap();
            if device_desc.vendor_id() == USB_VID && device_desc.product_id() == USB_PID {
                count += 1;
                let Ok(handler) = device.open() else {
                    continue;
                };

                println!("Bus {:03} Device {:03} ID {:04x}:{:04x} {} {} {}",
                    device.bus_number(),
                    device.address(),
                    device_desc.vendor_id(),
                    device_desc.product_id(),
                    handler.read_manufacturer_string_ascii(&device_desc).unwrap_or_default(),
                    handler.read_product_string_ascii(&device_desc).unwrap_or_default(),
                    handler.read_serial_number_string_ascii(&device_desc).unwrap_or_default()
                );
            }
        }

        if count == 0 {
            println!("No Sirins found.");
        }
    }

    fn connect() -> Result<Self, Error> {
        for device in rusb::devices().unwrap().iter() {
            println!("Does this ever run 1");
            let device_desc = device.device_descriptor().unwrap();
            println!("Does this ever run 2");
            println!("Device VID: {}", device_desc.vendor_id());
            println!("Device PID: {}", device_desc.product_id());
            if device_desc.vendor_id() == USB_VID && device_desc.product_id() == USB_PID {
                println!("Does this ever run 3");
                return Ok(Self {
                    device
                })
            }
        }
        println!("Does this ever run 4");
        return Err(Error::NoSirin)
    }

    pub fn open(&self) -> Result<SirinHandle, Error> {
        let dev = self.device.open()?;
        dev.claim_interface(0).unwrap();
        Ok(SirinHandle { dev })
    }
}

pub struct SirinHandle {
    dev: DeviceHandle<GlobalContext>
}

impl SirinHandle {
    pub fn receive_packet(&self) -> Result<OutPacket, Error> {
        let mut buf = [0u8; MAX_OUT_PACKET_SIZE * 128 * 100];
        let len = self.dev.read_bulk(USB_EP_IN_ADDR, &mut buf, Duration::from_secs(0))?;
        Ok(OutPacket::from_song(&buf[0..len])?)
    }

    pub fn send_packet(&self, packet: &InPacket) -> Result<(), Error> {
        let mut buf = [0u8; MAX_OUT_PACKET_SIZE];
        packet.to_song(&mut buf)?;
        println!("Attempting Writebulk!");
        self.dev.write_bulk(USB_EP_OUT_ADDR, &mut buf[0..packet.song_size()], Duration::from_secs(0))?;
        println!("WriteBulk worked!");
        Ok(())
    }

    pub fn receive_packets(&self) -> PacketIterator {
        PacketIterator::new(self)
    }
}

pub struct PacketIterator<'a> {
    handle: &'a SirinHandle,
    is_done: bool
}

impl <'a> PacketIterator<'a> {
    pub fn new(handle: &'a SirinHandle) -> Self {
        PacketIterator {
            handle,
            is_done: false
        }
    }
}

impl <'a> Iterator for PacketIterator<'a> {
    type Item = Result<OutPacket, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.is_done {
            return None
        }

        let packet = self.handle.receive_packet();

        if let Ok(OutPacket::Ok) = packet {
            self.is_done = true;
            None
        } else if let Ok(OutPacket::Null) = packet {
            self.is_done = true;
            println!("Packet was empty");
            exit(1)
        } else {
            Some(packet)
        }
    }
}