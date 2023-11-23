use std::collections::HashMap;
use std::fmt::{Display, Formatter};
use rusqlite::{params, Connection, Result};
use std::net::SocketAddr;
use clap::{Arg, ArgAction, Command, value_parser};
use env_logger::{Builder, Env};
use std::io::Write;
use std::process::exit;
use std::time::Duration;
use chrono::Local;

use http_body_util::Empty;
use hyper::body::Bytes;
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Request, Response};
use hyper_util::rt::TokioIo;
use tokio::net::TcpListener;
use hyper::StatusCode;
use http_body_util::{combinators::BoxBody, BodyExt};
use hyper::header::HeaderValue;
use serde::{Deserialize, Serialize};
use tokio::runtime::Runtime;
use tokio::sync::mpsc;
use tokio::sync::mpsc::error::TryRecvError;
use tokio::sync::mpsc::Sender;

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Hash, Debug, Default)]
struct WriteRequest {
    url: String,
    remote_ip: String,
    remote_port: u16,
    host: String,
    headers: String
}

#[derive(Ord, PartialOrd, Eq, PartialEq, Clone, Hash, Debug, Default, Serialize, Deserialize)]
struct SafeValue {
    value: String,
    hex: bool
}

impl Display for WriteRequest {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        f.write_fmt(format_args!("{} {}:{} {}", self.host, self.remote_ip, self.remote_port, self.url))
    }
}

fn header_value_to_string(value: &HeaderValue) -> SafeValue {
    match std::str::from_utf8(value.as_bytes()) {
        Ok(v) => SafeValue { value: v.to_owned(), hex: false },
        Err(_) => {
            // If the header value contains non-UTF-8 bytes, encode them to a hex string
            SafeValue { value: hex::encode(value.as_bytes()), hex: true }
        }
    }
}

// We create some utility functions to make Empty and Full bodies
// fit our broadened Response body type.
fn empty() -> BoxBody<Bytes, hyper::Error> {
    Empty::<Bytes>::new()
        .map_err(|never| match never {})
        .boxed()
}

async fn handle(req: Request<hyper::body::Incoming>, tx: Sender<WriteRequest>, ip: String, port: u16) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::Error> {
    let mut headers_map: HashMap<String, SafeValue> = HashMap::new();
    let headers = req.headers();
    log::trace!("Converting headers to map");
    for (key, value) in headers.iter() {
        let key_str = key.as_str().to_owned();
        let val = header_value_to_string(value);
        headers_map.insert(key_str, val);
    }
    log::trace!("Converting headers to json");
    match serde_json::to_string(&headers_map) {
        Ok(headers) => {
            log::trace!("Getting host header");
            match req.headers()[hyper::header::HOST].to_str() {
                Ok(host) => {
                    log::trace!("Sending to sqlite thread");
                    match tx.send(WriteRequest {
                        host: host.to_string(),
                        url: req.uri().to_string(),
                        remote_ip: ip,
                        remote_port: port,
                        headers
                    }).await {
                        Ok(_) => log::trace!("Successfully send data"),
                        Err(err) => log::error!("{}", err)
                    };
                },
                Err(err) => {
                    log::error!("{}", err);
                }
            }
        }
        Err(err) => log::error!("{}", err)
    }
    log::debug!("Responding with 404");
    let mut not_found = Response::new(empty());
    *not_found.status_mut() = StatusCode::NOT_FOUND;
    Ok(not_found)
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
        .get_matches();
    let port = *cmd.get_one::<u16>("port").expect("`port` should be non empty");
    let db: String = cmd.get_one::<String>("database").expect("`database` should be non empty").clone();
    let queuesize = *cmd.get_one::<usize>("queuesize").expect("`queuesize` should be non empty");
    Builder::from_env(Env::default())
        .format(|buf, record| {
            writeln!(
                buf,
                "{} [{}] {}: {}",
                Local::now().format("%Y-%m-%d %H:%M:%S%.6f"),
                record.target(),
                record.level(),
                record.args()
            )
        })
        .init();

    let addrs = [
        SocketAddr::from(([0, 0, 0, 0], port)),
        SocketAddr::from(([0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0], port)),
    ];
    log::trace!("Trying to listen on port {} on 0.0.0.0 and [::]", port);
    let listener = TcpListener::bind(&addrs[..]).await.unwrap();

    // main channel
    let (tx, mut rx) = mpsc::channel::<WriteRequest>(queuesize); // Channel for write requests
    // Control channel
    let (write_cache_tx, mut write_cache_rx) = mpsc::channel::<bool>(1); // Channel for write requests

    let cloned_write_cache_tx = write_cache_tx.clone();
    match ctrlc::set_handler(move || {
        let rt = Runtime::new().unwrap();
        let cloned = cloned_write_cache_tx.clone();
        rt.block_on(async move {
            match cloned.send(false).await {
                Ok(_) => log::debug!("Send last request to flush sqlite cache"),
                Err(err) => log::error!("Unable to send flush sqlite cache request: {}", err)
            }
        });
    }) {
        Ok(_) => log::debug!("ctrl c set"),
        Err(err) => log::error!("Unable to set ctrl c handler: {}", err)
    }

    std::thread::spawn(|| {
        let rt = Runtime::new().unwrap();
        rt.block_on(async move {
            log::debug!("Started thread to send cache write signal periodically");
            loop {
                tokio::time::sleep(Duration::new(60, 0)).await;
                match write_cache_tx.send(true).await {
                    Ok(_) => log::debug!("Send cache flush request"),
                    Err(err) => log::error!("{}", err)
                }
            }
        });
    });

    std::thread::spawn(|| {
        let rt = Runtime::new().unwrap();
        rt.block_on(async move {
            log::debug!("Started db connection task");
            log::trace!("Trying to open an sqlite conn to {}", db);
            let conn = match Connection::open(db) {
                Ok(con) => {
                    log::trace!("Opened Connection");
                    con
                },
                Err(err) => {
                    log::error!("{}", err);
                    return;
                }
            };
            log::trace!("Increasing cache");
            match conn.execute("PRAGMA cache_size = -200000;", []) {
                Ok(_) => {
                    log::trace!("cache_size = -200000");
                },
                Err(err) => {
                    log::error!("Unable to increase cache_size: {}", err);
                }
            };
            log::trace!("Setting journal mode to WAL");
            match conn.query_row("PRAGMA journal_mode = WAL;", [], |row| row.get::<usize, String>(0)) {
                Ok(_) => {
                    log::trace!("journal_mode = WAL");
                },
                Err(err) => {
                    log::error!("Unable to set journal_mode = WAL: {}", err);
                }
            };
            log::trace!("Switching synchronous off");
            match conn.execute("PRAGMA synchronous = OFF;", []) {
                Ok(_) => {
                    log::trace!("synchronous = OFF");
                },
                Err(err) => {
                    log::error!("Unable to set synchronous = OFF: {}", err);
                }
            };

            log::trace!("Creating table if necessary");
            match conn.execute("CREATE TABLE IF NOT EXISTS requests (id INTEGER PRIMARY KEY AUTOINCREMENT, url TEXT, remote_ip TEXT, remote_port INTEGER, host TEXT, headers TEXT)", [],) {
                Ok(_) => log::debug!("Finished create table"),
                Err(err) => {
                    log::error!("{}", err);
                    return;
                }
            };
            log::trace!("Waiting to receive requests over the channel");
            loop {
                let req = rx.try_recv();
                match req {
                    Ok(request) => {
                        log::trace!("Received write request, executing, data: {}", request);
                        // Process each write request
                        match conn.execute(
                            "INSERT INTO requests (url, remote_ip, remote_port, host, headers) VALUES (?1, ?2, ?3, ?4, ?5)",
                            params![request.url, request.remote_ip, request.remote_port, request.host, request.headers],
                        ) {
                            Ok(_) => log::trace!("Inserted request data into sqlite db"),
                            Err(err) => log::error!("{}", err)
                        };
                    }
                    Err(TryRecvError::Empty) => {
                        match write_cache_rx.try_recv() {
                            Ok(continue_exec) => {
                                log::debug!("Received cache flush request");
                                match conn.cache_flush() {
                                    Ok(_) => {
                                        log::info!("Wrote db cache to disk, bye bye");
                                    },
                                    Err(err) => {
                                        log::error!("Could not write db cache to disk! {}", err);
                                    }
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
                            },
                            Err(TryRecvError::Disconnected) => {
                                log::error!("Cache flush channel disconnected, flushing cache for good measure!");
                                match conn.cache_flush() {
                                    Ok(_) => {
                                        log::info!("Wrote db cache to disk, bye bye");
                                        break;
                                    },
                                    Err(err) => {
                                        log::error!("Could not write db cache to disk! {}", err);
                                        break;
                                    }
                                }
                            }
                        }
                    }
                    Err(TryRecvError::Disconnected) => {
                        log::error!("Main channel disconnected, flushing cache!");
                        match conn.cache_flush() {
                            Ok(_) => {
                                log::info!("Wrote db cache to disk, bye bye");
                                break;
                            },
                            Err(err) => {
                                log::error!("Could not write db cache to disk! {}", err);
                                break;
                            }
                        }
                    }
                }
            }
            // Successfully wrote the db, no messages in the queue, kill everything
            exit(0);
        });
    });

    // We start a loop to continuously accept incoming connections
    loop {
        log::trace!("Waiting for connection to be accepted");
        let (stream, socket) = listener.accept().await?;
        log::trace!("Accepted connection from {}:{}", socket.ip(), socket.port());
        let io = TokioIo::new(stream);

        let tx_clone = tx.clone();
        let service = service_fn(move |request| {
            let tx_clone_2 = tx_clone.clone(); // Clone for each invocation
            handle(request, tx_clone_2, socket.ip().to_string(), socket.port())
        });

        tokio::spawn(async move {
            if let Err(err) = http1::Builder::new().serve_connection(io, service).await {
                log::error!("Error serving connection: {:?}", err);
            }
        });
    }
}