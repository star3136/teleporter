use crate::teleport::TeleportFeatures;
use crate::teleport::TeleportInit;
use crate::*;
use byteorder::{LittleEndian, ReadBytesExt};
use std::fs;
use std::fs::OpenOptions;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};

/// Server function sets up a listening socket for any incoming connnections
pub fn run(opt: Opt) -> Result<(), Error> {
    // Bind to all interfaces on specified Port
    let listener = match TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, opt.port))) {
        Ok(l) => l,
        Err(s) => {
            println!("Error binding to port: {:?}", &opt.port);
            return Err(s);
        }
    };

    println!(
        "Teleporter Server {} listening for connections on 0.0.0.0:{}",
        VERSION, &opt.port
    );

    let recv_list = Arc::new(Mutex::new(Vec::<String>::new()));

    // Listen for incoming connections
    for stream in listener.incoming() {
        let s = match stream {
            Ok(s) => s,
            _ => continue,
        };
        // Receive connections in recv function
        let recv_list_clone = Arc::clone(&recv_list);
        thread::spawn(move || {
            recv(s, recv_list_clone).unwrap();
        });
    }

    Ok(())
}

fn send_ack(ack: TeleportInitAck, mut stream: &TcpStream) -> Result<(), Error> {
    // Encode and send response
    let serial_resp = ack.serialize();
    stream.write_all(&serial_resp)?;

    Ok(())
}

fn print_list(list: &MutexGuard<Vec<String>>) {
    print!("\rReceiving: {:?}", list);
    io::stdout().flush().unwrap();
}

/// Recv receives filenames and file data for a file
fn recv(mut stream: TcpStream, recv_list: Arc<Mutex<Vec<String>>>) -> Result<(), Error> {
    let start_time = Instant::now();
    let ip = stream.peer_addr().unwrap();
    let mut file: File;

    // Receive header first
    let mut name_buf: [u8; 4096] = [0; 4096];
    let len = stream.read(&mut name_buf)?;
    let fix = &name_buf[..len];
    let mut header = TeleportInit::new(TeleportFeatures::NewFile);
    if header.deserialize(&fix.to_vec()).is_err() {
        return Err(Error::new(
            ErrorKind::InvalidData,
            "Data received did not match expected",
        ));
    };

    let mut filename: String = header.filename.iter().cloned().collect::<String>();

    let version = Version::parse(VERSION).unwrap();
    let compatible =
        { version.major as u16 == header.version[0] && version.minor as u16 == header.version[1] };

    if !compatible {
        println!(
            "Error: Version mismatch from: {:?}! Us:{} Client:{:?}",
            ip, VERSION, header.version
        );
        let resp = TeleportInitAck::new(TeleportInitStatus::WrongVersion);
        return send_ack(resp, &stream);
    }

    // Remove any preceeding '/'
    if filename.starts_with('/') {
        filename.remove(0);
    }

    // Prohibit directory traversal
    filename = filename.replace("../", "");

    // Test if overwrite is false and file exists
    if header.features & TeleportFeatures::Overwrite as u32 != TeleportFeatures::Overwrite as u32
        && Path::new(&filename).exists()
    {
        println!(" => Refusing to overwrite file: {:?}", &filename);
        let resp = TeleportInitAck::new(TeleportInitStatus::NoOverwrite);
        return send_ack(resp, &stream);
    }

    // Create recursive dirs
    if fs::create_dir_all(Path::new(&filename).parent().unwrap()).is_err() {
        println!("Error: unable to create directories: {:?}", &filename);
        let resp = TeleportInitAck::new(TeleportInitStatus::NoPermission);
        return send_ack(resp, &stream);
    };

    // Open file for writing
    file = match OpenOptions::new().read(true).write(true).open(&filename) {
        Ok(f) => f,
        Err(_) => match File::create(&filename) {
            Ok(f) => f,
            Err(_) => {
                println!("Error: unable to create file: {:?}", &filename);
                let resp = TeleportInitAck::new(TeleportInitStatus::NoPermission);
                return send_ack(resp, &stream);
            }
        },
    };
    let meta = file.metadata()?;
    let mut perms = meta.permissions();
    perms.set_mode(header.chmod);
    if fs::set_permissions(&filename, perms).is_err() {
        println!("Could not set file permissions");
        let resp = TeleportInitAck::new(TeleportInitStatus::NoPermission);
        return send_ack(resp, &stream);
    };

    // Send ready for data ACK
    let mut resp = TeleportInitAck::new(TeleportInitStatus::Proceed);

    // If overwrite and file exists, build TeleportDelta
    let mut chunk_size = 5120;
    file.set_len(header.filesize)?;
    if meta.len() > 0 {
        resp = TeleportInitAck::new(TeleportInitStatus::Overwrite);
        resp.delta = match utils::calc_delta_hash(&file) {
            Ok(d) => {
                chunk_size = d.delta_size + 1024;
                Some(d)
            }
            _ => None,
        };
    }

    send_ack(resp, &stream)?;

    let mut recv_data = recv_list.lock().unwrap();
    recv_data.push(filename.clone());
    print_list(&recv_data);
    drop(recv_data);

    // Receive file data
    let mut peek: [u8; 4] = [0; 4];
    let mut received: u64 = 0;
    let mut data = Vec::<u8>::new();
    data.resize(chunk_size as usize, 0);
    loop {
        // Read from network connection
        stream.peek(&mut peek)?;
        let mut peek_buf: &[u8] = &peek;
        let chunk_len = peek_buf.read_u32::<LittleEndian>().unwrap() as usize + 4 + 8 + 1;
        let data_slice = &mut data[..chunk_len];
        let len = match stream.read_exact(data_slice) {
            Ok(_) => chunk_len,
            Err(_) => 0,
        };

        if len == 0 {
            if received == header.filesize {
                let duration = start_time.elapsed();
                let speed =
                    (header.filesize as f64 * 8.0) / duration.as_secs() as f64 / 1024.0 / 1024.0;
                println!(
                    " => Received file: {:?} (from: {:?} v{:?}) ({:.2?} @ {:.3} Mbps)",
                    &filename, ip, &header.version, duration, speed
                );
            } else {
                println!(" => Error receiving: {:?}", &filename);
            }
            break;
        }

        let mut chunk = TeleportData::new();
        chunk.deserialize(data_slice)?;

        // Seek to offset
        file.seek(SeekFrom::Start(chunk.offset))?;

        // Write received data to file
        let wrote = file.write(&chunk.data)?;

        if chunk.length as usize != wrote {
            println!(
                "Error writing to file: {:?} (read: {}, wrote: {}). Out of space?",
                &filename, chunk.length, wrote
            );
            break;
        }

        received = chunk.offset;
        received += chunk.length as u64;

        if received > header.filesize {
            println!(
                "Error: Received {} greater than filesize!",
                received - header.filesize
            );
            break;
        }
    }

    let mut recv_data = recv_list.lock().unwrap();
    recv_data.retain(|x| x != &filename);
    print_list(&recv_data);
    drop(recv_data);

    Ok(())
}
