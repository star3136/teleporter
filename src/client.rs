use crate::utils::print_updates;
use crate::*;

/// Client function sends filename and file data for each filepath
pub fn run(opt: Opt) -> Result<()> {
    println!("Teleport Client");

    // For each filepath in the input vector...
    for (num, item) in opt.input.iter().enumerate() {
        let filepath = item.to_str().unwrap();
        let filename = item.file_name().unwrap();


        // Validate file
        let file = match File::open(&filepath) {
            Ok(f) => f,
            Err(s) => return Err(s),
        };
        let meta = match file.metadata() {
            Ok(m) => m,
            Err(s) => return Err(s),
        };
        let header = TeleportInit {
            filenum: (num + 1) as u64,
            totalfiles: opt.input.len() as u64,
            filesize: meta.len(),
            filename: filename.to_str().unwrap().to_string(),
            chmod: meta.permissions().mode(),
            overwrite: opt.overwrite,
        };

        // Connect to server
        let addr = format!("{}:{}", opt.dest, opt.port);
        let mut stream = TcpStream::connect(
            addr.parse::<SocketAddr>()
                .expect(&format!("Error with dest: {}", addr)),
        )
        .expect(&format!("Error connecting to: {:?}", opt.dest));

        println!(
            "Sending file {}/{}: {:?}",
            header.filenum, header.totalfiles, header.filename
        );

        // Send header first
        let serial = serde_json::to_string(&header).unwrap();
        stream
            .write(&serial.as_bytes())
            .expect("Failed to write to stream");

        let recv = match recv_ack(&stream) {
            Some(t) => t,
            None => {
                println!("Receive TeleportResponse timed out");
                return Ok(());
            }
        };

        match recv.ack {
            TeleportStatus::NoOverwrite => {
                println!(
                    "The server refused to overwrite the file: {}",
                    &header.filename
                );
                continue;
            }
            TeleportStatus::NoPermission => {
                println!(
                    "The server does not have permission to write to this file: {}",
                    &header.filename
                );
                continue;
            }
            TeleportStatus::NoSpace => {
                println!(
                    "The server has no space available to write the file: {}",
                    &header.filename
                );
                continue;
            }
            _ => true,
        };

        // Send file data
        let _ = send(stream, file, header);

        println!(" done!");
    }
    Ok(())
}

fn recv_ack(mut stream: &TcpStream) -> Option<TeleportResponse> {
    let mut buf: [u8; 4096] = [0; 4096];

    // Receive ACK that the server is ready for data
    let len = stream
        .read(&mut buf)
        .expect("Failed to receive TeleportResponse");
    let fix = &buf[..len];
    let resp: TeleportResponse =
        serde_json::from_str(str::from_utf8(&fix).unwrap()).expect("Cannot parse TeleportResponse");

    Some(resp)
}

/// Send function receives the ACK for data and sends the file data
fn send(mut stream: TcpStream, mut file: File, header: TeleportInit) -> Result<()> {
    let mut buf: [u8; 4096] = [0; 4096];

    // Send file data
    let mut sent = 0;
    loop {
        // Read a chunk of the file
        let len = file.read(&mut buf).expect("Failed to read file");

        // If a length of 0 was read, we're done sending
        if len == 0 {
            break;
        }

        let data = &buf[..len];

        // Send that data chunk
        stream.write_all(&data).expect("Failed to send data");
        stream.flush().expect("Failed to flush");

        sent += len;
        print_updates(sent as f64, &header);
    }

    Ok(())
}
