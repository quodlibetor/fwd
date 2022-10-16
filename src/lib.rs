use anyhow::{bail, Result};
use log::LevelFilter;
use log::{debug, error, info, warn};
use message::{Message, MessageReader, MessageWriter};
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader,
    BufWriter,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::process;
use tokio::sync::mpsc;

mod message;
mod refresh;
mod ui;

// ----------------------------------------------------------------------------
// Write Management

/// Gathers writes from an mpsc queue and writes them to the specified
/// writer.
///
/// This is kind of an odd function. It raises a lot of questions.
///
/// *Why can't this just be a wrapper function on top of MessageWriter that
/// everybody calls?* Well, we could do that, but we also need to synchronize
/// writes to the underlying stream.
///
/// *Why not use an async mutex?* Because this function has a nice side
/// benefit: if it ever quits, we're *either* doing an orderly shutdown
/// (because the last write end of this channel closed) *or* the remote
/// connection has closed. [client_main] uses this fact to its advantage to
/// detect when the connection has failed.
///
/// At some point we may even automatically reconnect in response!
///
async fn pump_write<T: AsyncWrite + Unpin>(
    messages: &mut mpsc::Receiver<Message>,
    writer: &mut MessageWriter<T>,
) -> Result<()> {
    while let Some(msg) = messages.recv().await {
        writer.write(msg).await?;
    }
    Ok(())
}

// ----------------------------------------------------------------------------
// Server

async fn server_read<T: AsyncRead + Unpin>(
    reader: &mut MessageReader<T>,
    writer: mpsc::Sender<Message>,
) -> Result<()> {
    // info!("< Processing packets...");
    loop {
        let message = reader.read().await?;

        use Message::*;
        match message {
            Ping => (),
            Refresh => {
                let writer = writer.clone();
                tokio::spawn(async move {
                    let ports = match refresh::get_entries() {
                        Ok(ports) => ports,
                        Err(_e) => {
                            error!("< Error scanning: {:?}", _e);
                            vec![]
                        }
                    };
                    if let Err(_e) = writer.send(Message::Ports(ports)).await {
                        // Writer has been closed for some reason, we can just quit.... I hope everything is OK?
                        warn!("< Warning: Error sending: {:?}", _e);
                    }
                });
            }
            _ => panic!("Unsupported: {:?}", message),
        };
    }
}

async fn server_main<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin>(
    reader: &mut MessageReader<Reader>,
    writer: &mut MessageWriter<Writer>,
) -> Result<()> {
    // The first message we send must be an announcement.
    writer.write(Message::Hello(0, 1, vec![])).await?;

    // Jump into it...
    let (msg_sender, mut msg_receiver) = mpsc::channel(32);
    let writing = pump_write(&mut msg_receiver, writer);
    let reading = server_read(reader, msg_sender);
    tokio::pin!(reading);
    tokio::pin!(writing);

    let (mut done_writing, mut done_reading) = (false, false);
    loop {
        tokio::select! {
            result = &mut writing, if !done_writing => {
                done_writing = true;
                if let Err(e) = result {
                    return Err(e);
                }
                if done_reading && done_writing {
                    return Ok(());
                }
            },
            result = &mut reading, if !done_reading => {
                done_reading = true;
                if let Err(e) = result {
                    return Err(e);
                }
                if done_reading && done_writing {
                    return Ok(());
                }
            },
        }
    }
}

async fn client_sync<Read: AsyncRead + Unpin>(reader: &mut Read) -> Result<(), tokio::io::Error> {
    info!("Waiting for synchronization marker...");

    // Run these two loops in parallel; the copy of stdin should stop when
    // we've seen the marker from the client. If the pipe closes for whatever
    // reason then obviously we quit.
    let mut stdout = tokio::io::stdout();
    tokio::select! {
        result = async {
            let mut seen = 0;
            while seen < 8 {
                let byte = reader.read_u8().await?;
                if byte == 0 {
                    seen += 1;
                } else {
                    stdout.write_u8(byte).await?;
                }
            }

            Ok::<_, tokio::io::Error>(())
        } => result,
    }
}

/// Handle an incoming client connection, by forwarding it to the SOCKS5
/// server at the specified port.
///
/// This contains a very simplified implementation of a SOCKS5 connector,
/// enough to work with the SSH I have. I would have liked it to be SOCKS4,
/// which is a much simpler protocol, but somehow it didn't work.
async fn client_handle_connection(socks_port: u16, port: u16, socket: TcpStream) -> Result<()> {
    debug!("Handling connection!");

    let dest_addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, socks_port);
    let mut dest_socket = TcpStream::connect(dest_addr).await?;

    debug!("Connected, sending handshake request");
    let packet: [u8; 3] = [
        0x05, // v5
        0x01, // 1 auth method
        0x00, // my one auth method is no auth
    ];
    dest_socket.write_all(&packet[..]).await?;
    debug!("Initial handshake sent. Awaiting handshake response");

    let mut response: [u8; 2] = [0; 2];
    dest_socket.read_exact(&mut response).await?;
    if response[0] != 0x05 {
        bail!("SOCKS incorrect response version {}", response[0]);
    }
    if response[1] == 0xFF {
        bail!("SOCKS server says no acceptable auth");
    }
    if response[1] != 0x00 {
        bail!("SOCKS server chose something wild? {}", response[1]);
    }

    debug!("Handshake response received, sending connect request");
    let packet: [u8; 10] = [
        0x05,                                       // version again :P
        0x01,                                       // connect
        0x00,                                       // reserved!
        0x01,                                       // ipv4
        127,                                        // lo
        0,                                          // ..ca..
        0,                                          // ..lho..
        1,                                          // ..st
        ((port & 0xFF00) >> 8).try_into().unwrap(), // port (high)
        ((port & 0x00FF) >> 0).try_into().unwrap(), // port (low)
    ];
    dest_socket.write_all(&packet[..]).await?;

    debug!("Connect request sent, awaiting response");
    let mut response: [u8; 4] = [0; 4];
    dest_socket.read_exact(&mut response).await?;
    if response[0] != 0x05 {
        bail!("SOCKS5 incorrect response version again? {}", response[0]);
    }
    if response[1] != 0x00 {
        bail!("SOCKS5 reports a connect error {}", response[1]);
    }
    // Now we 100% do not care about the following information but we must
    // discard it so we can get to the good stuff. response[3] is the type of address...
    if response[3] == 0x01 {
        // IPv4 - 4 bytes.
        let mut response: [u8; 4] = [0; 4];
        dest_socket.read_exact(&mut response).await?;
    } else if response[3] == 0x03 {
        // Domain Name
        let len = dest_socket.read_u8().await?;
        for _ in 0..len {
            dest_socket.read_u8().await?; // So slow!
        }
    } else if response[3] == 0x04 {
        // IPv6 - 8 bytes
        let mut response: [u8; 8] = [0; 8];
        dest_socket.read_exact(&mut response).await?;
    } else {
        bail!(
            "SOCKS5 sent me an address I don't understand {}",
            response[3]
        );
    }
    // Finally the port number. Again, garbage, but it's in the packet we need to skip.
    let mut response: [u8; 2] = [0; 2];
    dest_socket.read_exact(&mut response).await?;

    info!("Connection established on port {}", port);

    let (client_read_half, client_write_half) = socket.into_split();
    let (server_read_half, server_write_half) = dest_socket.into_split();
    let client_to_server = tokio::spawn(async move {
        let mut client_read_half = client_read_half;
        let mut server_write_half = server_write_half;
        tokio::io::copy(&mut client_read_half, &mut server_write_half).await
    });
    let server_to_client = tokio::spawn(async move {
        let mut server_read_half = server_read_half;
        let mut client_write_half = client_write_half;
        tokio::io::copy(&mut server_read_half, &mut client_write_half).await
    });

    let client_err = client_to_server.await;
    debug!("Done client -> server");
    let svr_err = server_to_client.await;
    debug!("Done server -> client");

    if let Ok(Err(e)) = client_err {
        return Err(e.into());
    } else if let Ok(Err(e)) = svr_err {
        return Err(e.into());
    }

    Ok(())
}

async fn client_listen(port: u16, socks_port: u16) -> Result<()> {
    loop {
        let listener = TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port)).await?;
        loop {
            // The second item contains the IP and port of the new
            // connection, but we don't care.
            let (socket, _) = listener.accept().await?;

            tokio::spawn(async move {
                if let Err(e) = client_handle_connection(socks_port, port, socket).await {
                    error!("Error handling connection: {:?}", e);
                } else {
                    debug!("Done???");
                }
            });
        }
    }
}

async fn client_read<T: AsyncRead + Unpin>(
    reader: &mut MessageReader<T>,
    events: mpsc::Sender<ui::UIEvent>,
) -> Result<()> {
    info!("Running");
    loop {
        let message = reader.read().await?;
        // info!("> packet {:?}", message); // TODO: Smaller

        use Message::*;
        match message {
            Ping => (),
            Ports(ports) => {
                if let Err(_) = events.send(ui::UIEvent::Ports(ports)).await {
                    // TODO: Log
                }
            }
            _ => panic!("Unsupported: {:?}", message),
        };
    }
}

async fn client_pipe_stderr<Debug: AsyncBufRead + Unpin>(
    debug: &mut Debug,
    events: mpsc::Sender<ui::UIEvent>,
) {
    loop {
        let mut line = String::new();
        match debug.read_line(&mut line).await {
            Err(e) => {
                error!("Error reading stderr from server: {:?}", e);
                break;
            }
            Ok(0) => {
                warn!("stderr stream closed");
                break;
            }
            _ => {
                _ = events.send(ui::UIEvent::ServerLine(line)).await;
            }
        }
    }
}

async fn client_main<Reader: AsyncRead + Unpin, Writer: AsyncWrite + Unpin>(
    socks_port: u16,
    reader: &mut MessageReader<Reader>,
    writer: &mut MessageWriter<Writer>,
    events: mpsc::Sender<ui::UIEvent>,
) -> Result<()> {
    // Wait for the server's announcement.
    if let Message::Hello(major, minor, _) = reader.read().await? {
        if major != 0 || minor > 1 {
            bail!("Unsupported remote protocol version {}.{}", major, minor);
        }
    } else {
        bail!("Expected a hello message from the remote server");
    }

    // And now really get into it...
    let (msg_sender, mut msg_receiver) = mpsc::channel(32);

    _ = events.send(ui::UIEvent::Connected(socks_port)).await;

    let writing = pump_write(&mut msg_receiver, writer);
    let reading = client_read(reader, events);
    tokio::pin!(reading);
    tokio::pin!(writing);

    let (mut done_writing, mut done_reading) = (false, false);
    while !(done_reading && done_writing) {
        tokio::select! {
            result = async {
                loop {
                    use tokio::time::{sleep, Duration};
                    if let Err(e) = msg_sender.send(Message::Refresh).await {
                        break Err::<(), _>(e);
                    }
                    sleep(Duration::from_millis(500)).await;
                }
            }, if !done_writing => {
                if let Err(e) = result {
                    return Err(e.into());
                }
            },
            result = &mut writing, if !done_writing => {
                done_writing = true;
                if let Err(e) = result {
                    return Err(e);
                }
            },
            result = &mut reading, if !done_reading => {
                done_reading = true;
                if let Err(e) = result {
                    return Err(e);
                }
            },
        }
    }
    Ok(())
}

/////

pub async fn run_server() {
    let reader = BufReader::new(tokio::io::stdin());
    let mut writer = BufWriter::new(tokio::io::stdout());

    // Write the 8-byte synchronization marker.
    // eprintln!("< Writing marker...");
    writer
        .write_u64(0x00_00_00_00_00_00_00_00)
        .await
        .expect("Error writing marker");

    if let Err(_) = writer.flush().await {
        // eprintln!("Error writing sync marker: {:?}", e);
        return;
    }
    // eprintln!("< Done!");

    let mut writer = MessageWriter::new(writer);
    let mut reader = MessageReader::new(reader);
    if let Err(_) = server_main(&mut reader, &mut writer).await {
        // eprintln!("Error: {:?}", e);
    }
}

async fn spawn_ssh(server: &str) -> Result<(tokio::process::Child, u16), std::io::Error> {
    let socks_port = {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        listener.local_addr()?.port()
    };

    let mut cmd = process::Command::new("ssh");
    cmd.arg("-T")
        .arg("-D")
        .arg(socks_port.to_string())
        .arg(server)
        .arg("fwd")
        .arg("--server");

    cmd.stdout(std::process::Stdio::piped());
    cmd.stdin(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    let child = cmd.spawn()?;
    Ok((child, socks_port))
}

async fn client_connect_loop(remote: &str, events: mpsc::Sender<ui::UIEvent>) {
    loop {
        _ = events.send(ui::UIEvent::Disconnected).await;

        let (mut child, socks_port) = spawn_ssh(remote).await.expect("failed to spawn");

        let mut stderr = BufReader::new(
            child
                .stderr
                .take()
                .expect("child did not have a handle to stderr"),
        );

        let writer = child
            .stdin
            .take()
            .expect("child did not have a handle to stdin");

        let mut reader = BufReader::new(
            child
                .stdout
                .take()
                .expect("child did not have a handle to stdout"),
        );

        if let Err(e) = client_sync(&mut reader).await {
            error!("Error synchronizing: {:?}", e);
            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            continue;
        }

        let mut writer = MessageWriter::new(BufWriter::new(writer));
        let mut reader = MessageReader::new(reader);

        let sec = events.clone();
        tokio::spawn(async move {
            client_pipe_stderr(&mut stderr, sec).await;
        });

        if let Err(e) = client_main(socks_port, &mut reader, &mut writer, events.clone()).await {
            error!("Server disconnected with error: {:?}", e);
        } else {
            warn!("Disconnected from server, reconnecting...");
        }
    }
}

pub async fn run_client(remote: &str) {
    let (event_sender, event_receiver) = mpsc::channel(1024);
    _ = log::set_boxed_logger(ui::Logger::new(event_sender.clone()));
    log::set_max_level(LevelFilter::Info);

    let mut ui = ui::UI::new(event_receiver);

    // Start the reconnect loop.
    tokio::select! {
        _ = ui.run() => (),
        _ = client_connect_loop(remote, event_sender) => ()
    }
}
