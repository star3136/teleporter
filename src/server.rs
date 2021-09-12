use crate::crypto::decrypt;
use crate::crypto::{calc_secret, genkey};
use crate::utils::print_updates;
use crate::*;

/// Server function sets up a listening socket for any incoming connnections
pub fn run(opt: Opt) -> Result<()> {
    // Bind to all interfaces on specified Port
    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::UNSPECIFIED, opt.port)))
        .expect(&format!("Error binding to port: {:?}", opt.port));

    // Listen for incoming connections
    for stream in listener.incoming() {
        // Receive connections in recv function
        thread::spawn(move || {
            recv(stream.unwrap()).unwrap();
        });
    }

    Ok(())
}

/// Recv receives filenames and file data for a file
fn recv(mut stream: TcpStream) -> Result<()> {
    let ip = stream.peer_addr().unwrap();

    let (private, public) = genkey();

    // Receive header first
    let mut name_buf: [u8; 4096] = [0; 4096];
    let len = stream.read(&mut name_buf)?;
    let fix = &name_buf[..len];
    let header: TeleportInit =
        serde_json::from_str(str::from_utf8(&fix).unwrap()).expect("Cannot understand filename");
    println!(
        "Receiving file {}/{}: {:?} (from {})",
        header.filenum, header.totalfiles, header.filename, ip
    );

    // Open file for writing
    let mut file = File::create(&header.filename).expect("Could not open file");

    // Send ready for data ACK
    let resp = TeleportResponse {
        ack: TeleportStatus::Proceed,
        pubkey: public,
    };
    let serial_resp = serde_json::to_string(&resp).unwrap();
    stream
        .write(&serial_resp.as_bytes())
        .expect("Failed to write to stream");

    let secret = calc_secret(&header.pubkey, &private);

    // Receive file data
    let mut buf: [u8; 4124] = [0; 4124];
    let mut received: u64 = 0;
    loop {
        // Read from network connection
        let mut input: Vec<u8>;
        match stream.read_exact(&mut buf) {
            Err(ref e) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                if header.filesize - received == 0 {
                    // A receive of length 0 means the transfer is complete
                    println!(" done!");
                    break;
                } else if header.filesize - received > 4096 {
                    println!("Error: Transfer truncated");
                    return Ok(());
                }
                input = buf.to_vec();
                input.truncate((header.filesize - received + 12 + 16) as usize);
            }
            Err(e) => return Err(e),
            _ => input = buf.to_vec(),
        };

        // Decrypt data
        let decrypted = match decrypt(&secret, input) {
            Ok(d) => d,
            Err(s) => {
                println!("Error in decrypt: {}", s);
                return Ok(());
            }
        };

        // Write received data to file
        let wrote = file.write(&decrypted).expect("Failed to write to file");
        if decrypted.len() != wrote {
            println!("Error writing to file: {}", &header.filename);
            break;
        }

        received += decrypted.len() as u64;
        print_updates(received as f64, &header);
    }

    Ok(())
}