use crate::errors::TeleportError;
use crate::teleport::{TeleportAction, TeleportEnc, TeleportFeatures, TeleportStatus};
use crate::teleport::{TeleportData, TeleportDelta, TeleportInit, TeleportInitAck};
use crate::ListenOpt;
use crate::VERSION;
use crate::{crypto, utils};
use semver::Version;
use std::fs;
use std::fs::{File, OpenOptions};
use std::io;
use std::io::{Seek, SeekFrom, Write};
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, TcpListener, TcpStream};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::Instant;

/// Server function sets up a listening socket for any incoming connnections
pub fn run(opt: ListenOpt) -> Result<(), TeleportError> {
    // Bind to all interfaces on specified Port
    let listener = match TcpListener::bind(SocketAddr::from((Ipv6Addr::UNSPECIFIED, opt.port))) {
        Ok(l) => l,
        Err(_) => match TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, opt.port))) {
            Ok(l) => l,
            Err(s) => {
                println!(
                    "Cannot bind to port: {}. Is Teleporter already running?",
                    &opt.port
                );
                return Err(TeleportError::Io(s));
            }
        },
    };

    // Print welcome banner
    println!(
        "Teleporter Server {} listening for connections on 0.0.0.0:{}",
        VERSION, &opt.port
    );

    // Print warning banner for dangerous options
    if opt.allow_dangerous_filepath {
        println!("Warning: `--allow-dangerous-filepath` is ENABLED. This is a potentially dangerous option, use at your own risk!");
    }

    let recv_list = Arc::new(Mutex::new(Vec::<String>::new()));

    // Listen for incoming connections
    for stream in listener.incoming() {
        let args = opt.clone();
        let s = match stream {
            Ok(s) => s,
            _ => continue,
        };

        // Receive connections in recv function
        let recv_list_clone = Arc::clone(&recv_list);
        thread::spawn(move || {
            if let Err(e) = handle_connection(s, &recv_list_clone, args) {
                println!("Error: {e:?}");
            }
            let recv_list = recv_list_clone
                .lock()
                .expect("Fatal error locking recv_list_clone");
            print_list(&recv_list);
        });
    }

    Ok(())
}

fn send_ack(
    ack: TeleportInitAck,
    stream: &mut TcpStream,
    enc: &Option<TeleportEnc>,
) -> Result<(), TeleportError> {
    // Encode and send response
    utils::send_packet(stream, TeleportAction::InitAck, enc, ack.serialize()?)
}

fn print_list(list: &MutexGuard<Vec<String>>) {
    if list.len() == 0 {
        print!("\rListening...");
    } else {
        print!("\rReceiving: {list:?}");
    }
    io::stdout().flush().expect("Fatal error flushing stdout");
}

fn rm_filename_from_list(filename: &str, list: &Arc<Mutex<Vec<String>>>) {
    let mut recv_data = list.lock().expect("Fatal error locking file list");
    recv_data.retain(|x| x != filename);
}

fn handle_connection(
    mut stream: TcpStream,
    recv_list: &Arc<Mutex<Vec<String>>>,
    opt: ListenOpt,
) -> Result<(), TeleportError> {
    let start_time = Instant::now();
    let ip = stream.peer_addr()?;

    let mut enc: Option<TeleportEnc> = None;

    // Receive header first
    let mut packet = utils::recv_packet(&mut stream, &None)?;
    if packet.action == TeleportAction::Ping as u8 {
        let mut ping = TeleportInit::default();
        ping.deserialize(&packet.data)?;
        if !TeleportFeatures::Ping.check_u32(ping.features) {
            return Ok(());
        }
        println!(
            "\rPing received from Teleporter v{} at {}",
            ping.version, ip
        );
        let pong = TeleportInitAck::new(TeleportStatus::Pong);
        return utils::send_packet(
            &mut stream,
            TeleportAction::PingAck,
            &None,
            pong.serialize()?,
        );
    } else if packet.action == TeleportAction::Ecdh as u8 {
        let mut ctx = TeleportEnc::new();
        let privkey = crypto::genkey(&mut ctx);
        ctx.deserialize(&packet.data)?;
        ctx.calc_secret(privkey);
        utils::send_packet(&mut stream, TeleportAction::EcdhAck, &None, ctx.serialize())?;
        enc = Some(ctx);
        packet = utils::recv_packet(&mut stream, &enc)?;
    } else if opt.must_encrypt {
        let resp = TeleportInitAck::new(TeleportStatus::RequiresEncryption);
        return send_ack(resp, &mut stream, &enc);
    }

    let mut header = TeleportInit::new(TeleportFeatures::NewFile);
    header.deserialize(&packet.data)?;

    if packet.action != TeleportAction::Init as u8 {
        let resp = TeleportInitAck::new(TeleportStatus::EncryptionError);
        return send_ack(resp, &mut stream, &enc);
    }

    let username = String::from_utf8(header.username)?;
    println!("username: {}", &username);
    let mut filename: String = String::from_utf8(header.filename)?;
    let features: u32 = header.features;

    let version = Version::parse(VERSION).expect("Fatal version error");
    let compatible = header.version.is_compatible(&version);

    if !compatible {
        println!(
            "Error: Version mismatch from: {:?}! Us:{} Client:{}",
            ip, VERSION, header.version
        );
        let resp = TeleportInitAck::new(TeleportStatus::WrongVersion);
        return send_ack(resp, &mut stream, &enc);
    }

    if !opt.allow_dangerous_filepath {
        if filename.starts_with('/') {
            // Remove any preceeding '/'
            filename.remove(0);
        }

        // Prohibit directory traversal
        filename = filename.replace("../", "");
    }

    if TeleportFeatures::Rename.check_u32(features) {
        let mut num = 1;
        let mut dest = filename.clone();
        while Path::new(&dest).exists() {
            dest = filename.clone() + "." + &num.to_string();
            num += 1;
        }
        filename = dest;
    }

    // Test if overwrite is false and file exists
    if !TeleportFeatures::Overwrite.check_u32(features) && Path::new(&filename).exists() {
        println!(" => Refusing to overwrite file: {}", &filename);
        let resp = TeleportInitAck::new(TeleportStatus::NoOverwrite);
        return send_ack(resp, &mut stream, &enc);
    }

    // Create recursive dirs
    let path = match Path::new(&filename).parent() {
        Some(p) => p,
        None => {
            println!(
                "Error: unable to parse the path and filename: {}",
                &filename
            );
            let resp = TeleportInitAck::new(TeleportStatus::BadFileName);
            return send_ack(resp, &mut stream, &enc);
        }
    };

    if fs::create_dir_all(path).is_err() {
        println!("Error: unable to create directories: {}", &path.display());
        let resp = TeleportInitAck::new(TeleportStatus::NoPermission);
        return send_ack(resp, &mut stream, &enc);
    };

    // Open file for writing
    let mut file = match OpenOptions::new().read(true).write(true).open(&filename) {
        Ok(f) => {
            if TeleportFeatures::Backup.check_u32(features) {
                let dest = filename.clone() + ".bak";
                fs::copy(&filename, &dest)?;
            }
            f
        }
        Err(_) => match File::create(&filename) {
            Ok(f) => f,
            Err(_) => {
                println!("Error: unable to create file: {}", &filename);
                let resp = TeleportInitAck::new(TeleportStatus::NoPermission);
                return send_ack(resp, &mut stream, &enc);
            }
        },
    };
    let meta = file.metadata()?;
    let mut perms = meta.permissions();
    perms.set_mode(header.chmod);
    if fs::set_permissions(&filename, perms).is_err() {
        println!("Could not set file permissions");
        let resp = TeleportInitAck::new(TeleportStatus::NoPermission);
        return send_ack(resp, &mut stream, &enc);
    };

    // Send ready for data ACK
    let mut resp = TeleportInitAck::new(TeleportStatus::Proceed);
    TeleportFeatures::NewFile.add(&mut resp.features)?;

    // Add file to list
    let mut recv_data = recv_list.lock().expect("Fatal error locking recv_list");
    recv_data.push(filename.clone());
    print_list(&recv_data);
    drop(recv_data);

    // If overwrite and file exists, build TeleportDelta
    file.set_len(header.filesize)?;
    if meta.len() > 0 {
        TeleportFeatures::Overwrite.add(&mut resp.features)?;
        if TeleportFeatures::Delta.check_u32(features) {
            TeleportFeatures::Delta.add(&mut resp.features)?;
            resp.delta = match TeleportDelta::delta_hash(&file) {
                Ok(d) => Some(d),
                _ => None,
            };
        }
    }

    match send_ack(resp, &mut stream, &enc) {
        Ok(_) => (),
        Err(e) => {
            println!(
                "Connection closed (reason: {:?}). Aborted {} transfer.",
                e, &filename
            );
            rm_filename_from_list(&filename, recv_list);
            return Ok(());
        }
    }

    // Receive file data
    let mut received: u64 = 0;
    loop {
        // Read from network connection
        let packet = match utils::recv_packet(&mut stream, &enc) {
            Ok(s) => s,
            Err(e) => {
                println!(
                    "Connection closed (reason: {:?}). Aborted {} transfer.",
                    e, &filename
                );
                break;
            }
        };
        let mut chunk = TeleportData::new();
        chunk.deserialize(&packet.data)?;

        if chunk.data_len == 0 {
            if received == header.filesize
                || (header.filesize == chunk.offset && chunk.data_len == 0)
            {
                let duration = start_time.elapsed();
                let speed =
                    (header.filesize as f64 * 8.0) / duration.as_secs() as f64 / 1024.0 / 1024.0;
                println!(
                    " => Received file: {} (from: {} v{}) ({:.2?} @ {:.3} Mbps)",
                    &filename, ip, &header.version, duration, speed
                );
            } else {
                println!(" => Error receiving: {}", &filename);
            }
            break;
        }

        // Seek to offset
        file.seek(SeekFrom::Start(chunk.offset))?;

        // Write received data to file
        let wrote = file.write(&chunk.data)?;

        if chunk.data_len as usize != wrote {
            println!(
                "Error writing to file: {} (read: {}, wrote: {}). Out of space?",
                &filename, chunk.data_len, wrote
            );
            break;
        }

        received = chunk.offset;
        received += chunk.data_len as u64;

        if received > header.filesize {
            println!(
                "Error: Received {} greater than filesize!",
                received - header.filesize
            );
            break;
        }
    }

    rm_filename_from_list(&filename, recv_list);

    Ok(())
}
