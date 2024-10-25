use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use rusqlite::{params, Connection, Error, Result};
use std::net::SocketAddr;
use clap::{Arg, ArgAction, Command, value_parser};
use env_logger::{Builder, Env};
use std::io::Write;
use std::process::exit;
use std::time::Duration;
use time::{format_description::well_known::Rfc3339, OffsetDateTime};

use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use hyper::StatusCode;
use hyper::header::HeaderValue;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::{Receiver, Sender, channel};

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Hash, Debug, Default)]
struct WriteRequest {
    url: String,
    remote_ip: String,
    remote_port: u16,
    headers: String,
}

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Hash, Debug, Default, Serialize, Deserialize)]
struct SafeValue {
    value: String,
    hex: bool,
}

impl Display for WriteRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{}:{} {}", self.remote_ip, self.remote_port, self.url))
    }
}

#[inline]
fn header_value_to_string(value: &HeaderValue) -> SafeValue {
    match value.to_str() {
        Ok(v) => SafeValue { value: v.to_owned(), hex: false },
        Err(_) => {
            // If the header value contains non-UTF-8 bytes, encode them to a hex string
            SafeValue { value: hex::encode(value.as_bytes()), hex: true }
        }
    }
}

async fn handle(req: Request<hyper::body::Incoming>, tx: Sender<WriteRequest>, ip: String, port: u16) -> Result<Response<Empty<Bytes>>, hyper::Error> {
    log::trace!("Converting headers to map");
    let headers_map: HashMap<String, SafeValue> = req.headers().iter().map(|(key, value)| {
        let key_str = key.as_str().to_owned();
        let val = header_value_to_string(value);
        (key_str, val)
    }).collect();
    log::trace!("Converting headers to json");
    match serde_json::to_string(&headers_map) {
        Ok(headers) => {
            log::trace!("Sending to sqlite thread");
            match tx.send(WriteRequest {
                url: req.uri().to_string(),
                remote_ip: ip,
                remote_port: port,
                headers,
            }).await {
                Ok(_) => log::trace!("Successfully send data to channel"),
                Err(err) => log::error!("Cannot send request to db task: {}", err)
            };
        }
        Err(err) => log::error!("Cannot convert headers to json: {}", err)
    }
    log::debug!("Responding with 404");
    Ok(Response::builder().status(StatusCode::NOT_FOUND).body(Empty::<Bytes>::new()).unwrap())
}

fn set_db_options(conn: &mut Connection) {
    log::trace!("Increasing cache");
    match conn.execute("PRAGMA cache_size = -200000;", []) {
        Ok(_) => log::trace!("cache_size = -200000"),
        Err(err) => log::error!("Unable to increase cache_size: {}", err)
    };
    log::trace!("Setting journal mode to WAL");
    match conn.query_row("PRAGMA journal_mode = WAL;", [], |row| row.get::<usize, String>(0)) {
        Ok(_) => log::trace!("journal_mode = WAL"),
        Err(err) => log::error!("Unable to set journal_mode = WAL: {}", err)
    };
    log::trace!("Switching synchronous off");
    match conn.execute("PRAGMA synchronous = OFF;", []) {
        Ok(_) => log::trace!("synchronous = OFF"),
        Err(err) => log::error!("Unable to set synchronous = OFF: {}", err)
    };
}

async fn database(db: &str, mut rx: Receiver<WriteRequest>, mut write_cache_rx: Receiver<bool>) -> Result<(), Error> {
    log::debug!("Started db connection task");
    log::trace!("Trying to open an sqlite conn to {}", db);
    let mut conn = match Connection::open(db) {
        Ok(con) => {
            log::trace!("Opened Connection");
            con
        }
        Err(err) => {
            log::error!("Failed to open database: {}", err);
            return Err(err);
        }
    };
    set_db_options(&mut conn);

    log::trace!("Creating table if necessary");
    match conn.execute("CREATE TABLE IF NOT EXISTS requests (id INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT, remote_ip TEXT, remote_port INTEGER, headers TEXT)", []) {
        Ok(_) => log::debug!("Finished create table"),
        Err(err) => {
            log::error!("Failed to create requests table {}", err);
            return Err(err);
        }
    };
    log::trace!("Waiting to receive requests over the channel");
    // Squeeze a few nanoseconds out of not reconstructing the statement with each execution
    let mut prepared = match conn.prepare("INSERT INTO requests (url, remote_ip, remote_port, headers) VALUES (?1, ?2, ?3, ?4)") {
        Ok(prepped) => prepped,
        Err(err) => {
            log::error!("Cannot prepare requests insert statement: {}", err);
            return Err(err);
        }
    };

    loop {
        match rx.try_recv() {
            Ok(request) => {
                log::trace!("Received write request, executing, data: {}", request);
                // Process each write request
                match prepared.execute(params![request.url, request.remote_ip, request.remote_port, request.headers]) {
                    Ok(_) => log::trace!("Inserted request data into sqlite db"),
                    Err(err) => log::error!("{}", err)
                };
            }
            Err(TryRecvError::Empty) => {
                match write_cache_rx.try_recv() {
                    Ok(continue_exec) => {
                        log::debug!("Received cache flush request");
                        match conn.cache_flush() {
                            Ok(_) => log::info!("Wrote db cache to disk"),
                            Err(err) => log::error!("Could not write db cache to disk! {}", err)
                        }
                        if !continue_exec {
                            log::debug!("continue_exec not set, getting out of the loop");
                            break;
                        } else {
                            log::debug!("Continuing as normal")
                        }
                    }
                    Err(TryRecvError::Empty) => {
                        // if there is no work, no need to consume so much cpu
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    Err(TryRecvError::Disconnected) => {
                        log::error!("Cache flush channel disconnected, flushing cache for good measure!");
                        match conn.cache_flush() {
                            Ok(_) => log::info!("Wrote db cache to disk, bye bye"),
                            Err(err) => log::error!("Could not write db cache to disk! {}", err)
                        }
                        break;
                    }
                }
            }
            Err(TryRecvError::Disconnected) => {
                log::error!("Main channel disconnected, flushing cache!");
                match conn.cache_flush() {
                    Ok(_) => log::info!("Wrote db cache to disk, bye bye"),
                    Err(err) => log::error!("Could not write db cache to disk! {}", err)
                }
                break;
            }
        }
    }
    Ok(())
}

async fn write_signal_sender(write_cache_tx: Sender<bool>) {
    log::debug!("Started thread to send cache write signal periodically");
    loop {
        tokio::time::sleep(Duration::new(60, 0)).await;
        match write_cache_tx.send(true).await {
            Ok(_) => log::debug!("Send cache flush request"),
            Err(err) => log::error!("Failed to send cache flush request: {}", err)
        }
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let cmd = Command::new("http-honeypot")
        .version("1.0.0")
        .author("Rafael Sundorf <developer.rafael.sundorf@gmail.com>")
        .about("honeypot stuff!")
        .arg(Arg::new("port")
            .short('p')
            .long("port")
            .help("The port that the honeypot should run on")
            .long_help("The port that the honeypot should run on. Might need to execute 'setcap \"cap_net_bind_service=+ep\" /path/to/http-honeypot' to use ports under 1024 or run as root (not recommended)")
            .value_parser(value_parser!(u16))
            .action(ArgAction::Set)
            .default_value("80")
            .env("PORT")
        )
        .arg(Arg::new("database")
            .short('d')
            .long("database")
            .value_parser(value_parser!(String))
            .action(ArgAction::Set)
            .default_value("requests.sqlite")
            .env("DATABASE")
        )
        .arg(
            Arg::new("queuesize")
                .short('q')
                .long("queuesize")
                .value_parser(value_parser!(usize))
                .action(ArgAction::Set)
                .default_value("10000")
                .env("QUEUESIZE")
        )
        .arg(
            Arg::new("no_ipv6")
                .short('n')
                .long("no-v6")
                .action(ArgAction::SetTrue)
                .env("NOIPV6")
                .help("Do not listen on IPv6")
                .long_help("Disable listening on IPv6 interfaces")
        )
        .get_matches();
    let port = *cmd.get_one::<u16>("port").expect("`port` should be non empty");
    let db = cmd.get_one::<String>("database").expect("`database` should be non empty").clone();
    let queuesize = *cmd.get_one::<usize>("queuesize").expect("`queuesize` should be non empty");
    let no_ipv6 = cmd.get_flag("no_ipv6");
    Builder::from_env(Env::default())
        .format(|buf, record| {
            let now = OffsetDateTime::now_utc().format(&Rfc3339).unwrap();
            writeln!(
                buf,
                "{} [{}] {}: {}",
                now,
                record.target(),
                record.level(),
                record.args()
            )
        })
        .init();

    let listener = if no_ipv6 {
        let addrs = [
            SocketAddr::from(([0, 0, 0, 0], port)),
            SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], port)),
        ];
        log::trace!("Trying to listen on port {} on 0.0.0.0 and [::]", port);
        match TcpListener::bind(&addrs[..]).await {
            Ok(l) => {
                log::info!("Listening on port {} on 0.0.0.0 and [::]", port);
                l
            },
            Err(err) => {
                log::error!("Failed listen on ports: {}", err);
                exit(-1);
            }
        }
    } else {
        let addrs = [
            SocketAddr::from(([0, 0, 0, 0], port)),
        ];
        log::trace!("Trying to listen on port {} on 0.0.0.0", port);
        match TcpListener::bind(&addrs[..]).await {
            Ok(l) => {
                log::info!("Listening on port {} on 0.0.0.0", port);
                l
            },
            Err(err) => {
                log::error!("Failed listen on port: {}", err);
                exit(-1);
            }
        }
    };


    // main channel
    let (tx, rx) = channel::<WriteRequest>(queuesize); // Channel for write requests
    // Control channel
    let (write_cache_tx, write_cache_rx) = channel::<bool>(1); // Channel for write cache requests

    let cloned_write_cache_tx = write_cache_tx.clone();
    let ctrl_shutdown_write_cache_tx = write_cache_tx.clone();
    #[cfg(unix)]
    let usr1_write_cache_tx = write_cache_tx.clone();
    let db_cache_write_handle = tokio::spawn(async move {
        write_signal_sender(write_cache_tx).await;
    });

    tokio::spawn(async move {
        log::trace!("Registering and waiting for ctrl+c");
        match tokio::signal::ctrl_c().await { 
            Ok(_) => {
                log::info!("Received ctrl+c, aborting regular cache write");
                db_cache_write_handle.abort();
                match cloned_write_cache_tx.send(false).await {
                    Ok(_) => log::debug!("Send last request to flush sqlite cache"),
                    Err(err) => {
                        log::error!("Unable to send flush sqlite cache request: {}", err);
                        // Well fuck database thread panicked
                        exit(-1);
                    }
                }
            }
            Err(err) => {
                log::error!("Failed listening ctrl+c: {}", err);
            }
        }
    });

    #[cfg(windows)]
    tokio::spawn(async move {
        let mut shutdown = match tokio::signal::windows::ctrl_shutdown() {
            Ok(ctrl) => {
                ctrl
            }
            Err(err) => {
                log::error!("Failed listening ctrl+shutdown: {}", err);
                return;
            }
        };
        let mut logoff = match tokio::signal::windows::ctrl_logoff() {
            Ok(ctrl) => {
                ctrl
            }
            Err(err) => {
                log::error!("Failed listening ctrl+logoff: {}", err);
                return;
            }
        };
        let mut break_handler = match tokio::signal::windows::ctrl_break() {
            Ok(ctrl) => {
                ctrl
            }
            Err(err) => {
                log::error!("Failed listening ctrl+break: {}", err);
                return;
            }
        };
        let mut close = match tokio::signal::windows::ctrl_close() {
            Ok(ctrl) => {
                ctrl
            }
            Err(err) => {
                log::error!("Failed listening ctrl+close: {}", err);
                return;
            }
        };
        
        match tokio::select! {
            _ = shutdown.recv() => {}
            _ = logoff.recv() => {}
            _ = close.recv() => {}
            _ = break_handler.recv() => {}
        } { 
            _ => {
                match ctrl_shutdown_write_cache_tx.send(false).await {
                    Ok(_) => log::debug!("Send last request to flush sqlite cache"),
                    Err(err) => {
                        log::error!("Unable to send flush sqlite cache request after ctrl_shutdown/break/logoff/close: {}", err);
                        // Well fuck database thread panicked
                        exit(-1);
                    }
                }
            }
        };
    });

    #[cfg(unix)]
    tokio::spawn(async move {
        log::trace!("Trying to register SIG_TERM handler");
        let mut term_signal = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(term_signal) => {
                log::trace!("Registered SIG_TERM handler");
                term_signal
            },
            Err(err) => {
                log::error!("Failed listening term signal: {}", err);
                return;
            }
        };
        log::trace!("Trying to register SIG_HANG handler");
        let mut hangup_signal = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::hangup()) {
            Ok(hangup_signal) => {
                log::trace!("Registered SIG_HANGUP handler");
                hangup_signal
            },
            Err(err) => {
                log::error!("Failed listening hang signal: {}", err);
                return;
            }
        };
        log::trace!("Trying to register SIG_PIPE handler");
        let mut pipe_signal = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::pipe()) {
            Ok(pipe_signal) => {
                log::trace!("Registered SIG_PIPE handler");
                pipe_signal
            },
            Err(err) => {
                log::error!("Failed listening pipe signal: {}", err);
                return;
            }
        };
        log::trace!("Trying to register SIG_QUIT handler");
        let mut quit_signal = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::quit()) {
            Ok(quit_signal) => {
                log::trace!("Registered SIG_QUIT handler");
                quit_signal
            },
            Err(err) => {
                log::error!("Failed listening quit signal: {}", err);
                return;
            }
        };
        match tokio::select! {
            _ = term_signal.recv() => {}
            _ = hangup_signal.recv() => {}
            _ = pipe_signal.recv() => {}
            _ = quit_signal.recv() => {}
        } {
            _ => {
                log::trace!("Received either of term, hangup, pipe or quit");
                match ctrl_shutdown_write_cache_tx.send(false).await {
                    Ok(_) => log::debug!("Send last request to flush sqlite cache"),
                    Err(err) => {
                        log::error!("Unable to send flush sqlite cache request after receiving term/hangup/pipe/quit signal: {}", err);
                        // Well fuck database thread panicked
                        exit(-1);
                    }
                }
            }
        };
    });

    #[cfg(unix)]
    tokio::spawn(async move {
        log::trace!("Trying to register SIG_USR1 handler");
        let mut usr1_signal = match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()) {
            Ok(usr1_signal) => {
                log::trace!("Registered SIG_USR1 handler");
                usr1_signal
            },
            Err(err) => {
                log::error!("Failed listening term signal: {}", err);
                return;
            }
        };
        loop {
            match usr1_signal.recv().await {
                Some(_) => {
                    log::debug!("Sending request to flush sqlite cache because usr1 was received");
                    match usr1_write_cache_tx.send(true).await {
                        Ok(_) => log::debug!("Send request to flush sqlite cache"),
                        Err(err) => {
                            log::error!("Unable to send flush sqlite cache request after receiving usr1 signal: {}. Unable to receive further usr1 signals.", err);
                            break;
                        }
                    }
                }
                None => {
                    log::trace!("No usr1 signals can be received anymore");
                    break;
                }
            }
        }
    });

    std::thread::spawn(|| {
        // I spawnt a thread just for you, use it
        match tokio::runtime::Builder::new_current_thread().enable_time().build() {
            Ok(rt) => {
                rt.block_on(async move {
                    match database(&db, rx, write_cache_rx).await {
                        Ok(_) => {
                            log::info!("Database task finished");
                            exit(0);
                        },
                        Err(_) => {
                            log::error!("Database task finished with error");
                            // exit(-1)
                        }
                    };
                });
            }
            Err(err) => {
                log::error!("Failed to create database tokio runtime: {}", err);
                exit(-1)
            }
        }
    });

    // We start a loop to continuously accept incoming connections
    loop {
        log::trace!("Waiting for connection to be accepted");
        let (stream, socket) = match listener.accept().await {
            Ok((stream, socket)) => { (stream, socket) }
            Err(err) => {
                log::error!("Error accepting connection: {}", err);
                continue;
            }
        };
        log::trace!("Accepted connection from {}:{}", socket.ip(), socket.port());

        let tx_clone = tx.clone();
        let service = service_fn(move |request| {
            handle(request, tx_clone.clone(), socket.ip().to_string(), socket.port())
        });

        tokio::spawn(async move {
            if let Err(err) = http1::Builder::new().serve_connection(TokioIo::new(stream), service).await {
                log::error!("Error serving connection: {:?}", err);
            }
        });
    }
}