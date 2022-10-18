use crate::message::{Message, MessageReader, MessageWriter};
use anyhow::{bail, Result};
use bytes::BytesMut;
use log::LevelFilter;
use log::{debug, error, info, warn};
use std::net::{Ipv4Addr, SocketAddrV4};
use tokio::io::{
    AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt, AsyncWrite,
    AsyncWriteExt, BufReader, BufWriter,
};
use tokio::net::{TcpListener, TcpStream};
use tokio::process;
use tokio::sync::mpsc;

mod ui;

/// Wait for the server to be ready; we know the server is there and
/// listening when we see the special sync marker, which is 8 NUL bytes in a
/// row.
async fn client_sync<S: AsyncRead + Unpin, T: AsyncRead + Unpin>(
    reader: &mut S,
    client_stderr: &mut T,
) -> Result<(), tokio::io::Error> {
    info!("Waiting for synchronization marker...");

    let mut stderr = tokio::io::stderr();
    let mut stdout = tokio::io::stdout();
    let mut buf = BytesMut::with_capacity(1024);

    let mut seen = 0;
    let result = tokio::select! {
        result = async {
            loop {
                buf.clear();
                client_stderr.read_buf(&mut buf).await?;
                stderr.write_all(&buf[..]).await?;
            }
        } => result,
        result = async {
            while seen < 8 {
                let byte = reader.read_u8().await?;
                if byte == 0 {
                    seen += 1;
                } else {
                    stdout.write_u8(byte).await?;
                    seen = 0;
                }
            }

            Ok::<_, tokio::io::Error>(())
        } => result,
    };

    if let Err(_) = result {
        // Something went wrong, let's just make sure we flush the client's
        // stderr before we return.
        _ = stderr.write_all(&buf[..]).await;
        _ = tokio::io::copy(client_stderr, &mut stderr).await;
        _ = stderr.flush().await;
    }

    result
}

/// Handle an incoming client connection, by forwarding it to the SOCKS5
/// server at the specified port. This is the core of the entire thing.
///
/// This contains a very simplified implementation of a SOCKS5 connector,
/// enough to work with the SSH I have. I would have liked it to be SOCKS4,
/// which is a much simpler protocol, but somehow it didn't work.
async fn client_handle_connection(
    socks_port: u16,
    port: u16,
    socket: TcpStream,
) -> Result<()> {
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
        127,                                        // lo..
        0,                                          // ..cal..
        0,                                          // ..ho..
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
    // discard it so we can get to the good stuff. response[3] is the type of
    // address...
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
    // Finally the port number. Again, garbage, but it's in the packet we
    // need to skip.
    let mut response: [u8; 2] = [0; 2];
    dest_socket.read_exact(&mut response).await?;

    info!("Connection established on port {}", port);

    let mut socket = socket;
    tokio::io::copy_bidirectional(&mut socket, &mut dest_socket).await?;
    Ok(())
}

/// Listen on a port that we are currently forwarding, and use the SOCKS5
/// proxy on the specified port to handle the connections.
async fn client_listen(port: u16, socks_port: u16) -> Result<()> {
    loop {
        let listener =
            TcpListener::bind(SocketAddrV4::new(Ipv4Addr::LOCALHOST, port))
                .await?;
        loop {
            // The second item contains the IP and port of the new
            // connection, but we don't care.
            let (socket, _) = listener.accept().await?;

            tokio::spawn(async move {
                if let Err(e) =
                    client_handle_connection(socks_port, port, socket).await
                {
                    error!("Error handling connection: {:?}", e);
                } else {
                    debug!("Done???");
                }
            });
        }
    }
}

async fn client_handle_messages<T: AsyncRead + Unpin>(
    reader: &mut MessageReader<T>,
    events: mpsc::Sender<ui::UIEvent>,
) -> Result<()> {
    loop {
        use Message::*;
        match reader.read().await? {
            Ping => (),
            Ports(ports) => {
                if let Err(_) = events.send(ui::UIEvent::Ports(ports)).await {
                    // TODO: Log
                }
            }
            message => panic!("Unsupported: {:?}", message),
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
    _ = events.send(ui::UIEvent::Connected(socks_port)).await;

    tokio::select! {
        result = async {
            loop {
                use tokio::time::{sleep, Duration};
                if let Err(e) = writer.write(Message::Refresh).await {
                    break Err::<(), _>(e);
                }
                sleep(Duration::from_millis(500)).await;
            }
        } => {
            if let Err(e) = result {
                return Err(e.into());
            }
        },
        result = client_handle_messages(reader, events) => {
            if let Err(e) = result {
                return Err(e.into());
            }
        },
    }
    Ok(())
}

async fn spawn_ssh(
    server: &str,
) -> Result<(tokio::process::Child, u16), std::io::Error> {
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

#[cfg(target_family = "windows")]
fn is_sigint(status: std::process::ExitStatus) -> bool {
    match status.code() {
        Some(255) => true,
        _ => false,
    }
}

#[cfg(target_family = "unix")]
fn is_sigint(status: std::process::ExitStatus) -> bool {
    use std::os::unix::process::ExitStatusExt;
    match status.signal() {
        Some(2) => true,
        Some(_) => false,
        None => false,
    }
}

async fn client_connect_loop(remote: &str, events: mpsc::Sender<ui::UIEvent>) {
    loop {
        _ = events.send(ui::UIEvent::Disconnected).await;

        let (mut child, socks_port) =
            spawn_ssh(remote).await.expect("failed to spawn");

        let mut stderr = child
            .stderr
            .take()
            .expect("child did not have a handle to stderr");

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

        if let Err(e) = client_sync(&mut reader, &mut stderr).await {
            error!("Error synchronizing: {:?}", e);
            match child.wait().await {
                Ok(status) => {
                    if is_sigint(status) {
                        return;
                    } else {
                        match status.code() {
                            Some(127) => eprintln!("Cannot find `fwd` remotely, make sure it is installed"),
                            _ => (),
                        };
                    }
                }
                Err(_) => (),
            };

            tokio::time::sleep(tokio::time::Duration::from_secs(3)).await;
            continue;
        }

        let mut stderr = BufReader::new(stderr);
        let mut writer = MessageWriter::new(BufWriter::new(writer));
        let mut reader = MessageReader::new(reader);

        let sec = events.clone();
        tokio::spawn(async move {
            client_pipe_stderr(&mut stderr, sec).await;
        });

        if let Err(e) =
            client_main(socks_port, &mut reader, &mut writer, events.clone())
                .await
        {
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