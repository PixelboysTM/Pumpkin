pub mod plugin;

pub(crate) mod client;
pub(crate) mod command;
pub(crate) mod entity;
pub(crate) mod error;
pub(crate) mod proxy;
pub(crate) mod query;
pub(crate) mod rcon;
pub(crate) mod server;
pub(crate) mod world;

pub use pumpkin_core::*;
pub use server::{Server, CURRENT_MC_VERSION};

use client::Client;
use log::LevelFilter;
use server::ticker::Ticker;
use std::io::{self};
use tokio::io::{AsyncBufReadExt, BufReader};
#[cfg(not(unix))]
use tokio::signal::ctrl_c;
#[cfg(unix)]
use tokio::signal::unix::{signal, SignalKind};

use std::sync::Arc;

use pumpkin_config::{ADVANCED_CONFIG, BASIC_CONFIG};
use pumpkin_core::text::{color::NamedColor, TextComponent};
use rcon::RCONServer;
use std::time::Instant;

// Setup some tokens to allow us to identify which event is for which socket.

fn scrub_address(ip: &str) -> String {
    use pumpkin_config::BASIC_CONFIG;
    if BASIC_CONFIG.scrub_ips {
        ip.chars()
            .map(|ch| if ch == '.' || ch == ':' { ch } else { 'x' })
            .collect()
    } else {
        ip.to_string()
    }
}

fn init_logger() {
    use pumpkin_config::ADVANCED_CONFIG;
    if ADVANCED_CONFIG.logging.enabled {
        let mut logger = simple_logger::SimpleLogger::new();

        if !ADVANCED_CONFIG.logging.timestamp {
            logger = logger.without_timestamps();
        }

        if ADVANCED_CONFIG.logging.env {
            logger = logger.env();
        }

        logger = logger.with_level(convert_logger_filter(ADVANCED_CONFIG.logging.level));

        logger = logger.with_colors(ADVANCED_CONFIG.logging.color);
        logger = logger.with_threads(ADVANCED_CONFIG.logging.threads);
        logger.init().unwrap();
    }
}

const fn convert_logger_filter(level: pumpkin_config::logging::LevelFilter) -> LevelFilter {
    match level {
        pumpkin_config::logging::LevelFilter::Off => LevelFilter::Off,
        pumpkin_config::logging::LevelFilter::Error => LevelFilter::Error,
        pumpkin_config::logging::LevelFilter::Warn => LevelFilter::Warn,
        pumpkin_config::logging::LevelFilter::Info => LevelFilter::Info,
        pumpkin_config::logging::LevelFilter::Debug => LevelFilter::Debug,
        pumpkin_config::logging::LevelFilter::Trace => LevelFilter::Trace,
    }
}

pub async fn main() -> io::Result<()> {
    init_logger();
    // let rt = tokio::runtime::Builder::new_multi_thread()
    //     .enable_all()
    //     .build()
    //     .unwrap();

    tokio::spawn(async {
        setup_sighandler()
            .await
            .expect("Unable to setup signal handlers");
    });

    // ensure rayon is built outside of tokio scope
    rayon::ThreadPoolBuilder::new().build_global().unwrap();
    let default_panic = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        default_panic(info);
        // TODO: Gracefully exit?
        std::process::exit(1);
    }));

    let time = Instant::now();

    // Setup the TCP server socket.
    let listener = tokio::net::TcpListener::bind(BASIC_CONFIG.server_address)
        .await
        .expect("Failed to start TcpListener");
    // In the event the user puts 0 for their port, this will allow us to know what port it is running on
    let addr = listener
        .local_addr()
        .expect("Unable to get the address of server!");

    let use_console = ADVANCED_CONFIG.commands.use_console;
    let rcon = ADVANCED_CONFIG.rcon.clone();

    // Plugin setup
    let mut plugin_manager = plugin::PluginManager::new();
    plugin_manager.load_plugins();
    plugin_manager.init().await;
    let registry = plugin_manager.event_registry();
    registry.read().await.on_init(()).await;

    // registry
    //     .read()
    //     .await
    //     .on_init(&mut InitEventData {
    //         event_registry: &mut registry.write().await as &mut EventRegistry,
    //         commands: vec![],
    //     })
    //     .await;

    let server = Arc::new(Server::new());
    let mut ticker = Ticker::new(BASIC_CONFIG.tps);

    log::info!("Started Server took {}ms", time.elapsed().as_millis());
    log::info!("You now can connect to the server, Listening on {}", addr);

    if use_console {
        setup_console(server.clone());
    }
    if rcon.enabled {
        let server = server.clone();
        tokio::spawn(async move {
            RCONServer::new(&rcon, server).await.unwrap();
        });
    }

    if ADVANCED_CONFIG.query.enabled {
        log::info!("Query protocol enabled. Starting...");
        tokio::spawn(query::start_query_handler(server.clone(), addr));
    }

    {
        let server = server.clone();
        tokio::spawn(async move {
            ticker.run(&server).await;
        });
    }

    let mut master_client_id: u16 = 0;
    loop {
        // Asynchronously wait for an inbound socket.
        let (connection, address) = listener.accept().await?;

        if let Err(e) = connection.set_nodelay(true) {
            log::warn!("failed to set TCP_NODELAY {e}");
        }

        let id = master_client_id;
        master_client_id = master_client_id.wrapping_add(1);

        log::info!(
            "Accepted connection from: {} (id {})",
            scrub_address(&format!("{address}")),
            id
        );

        let client = Arc::new(Client::new(connection, addr, id));

        let server = server.clone();
        tokio::spawn(async move {
            while !client.closed.load(std::sync::atomic::Ordering::Relaxed)
                && !client
                    .make_player
                    .load(std::sync::atomic::Ordering::Relaxed)
            {
                let open = client.poll().await;
                if open {
                    client.process_packets(&server).await;
                };
            }
            if client
                .make_player
                .load(std::sync::atomic::Ordering::Relaxed)
            {
                let (player, world) = server.add_player(client).await;
                world
                    .spawn_player(&BASIC_CONFIG, player.clone(), &server.command_dispatcher)
                    .await;

                // poll Player
                while !player
                    .client
                    .closed
                    .load(core::sync::atomic::Ordering::Relaxed)
                {
                    let open = player.client.poll().await;
                    if open {
                        player.process_packets(&server).await;
                    };
                }
                log::debug!("Cleaning up player for id {}", id);
                player.remove().await;
                server.remove_player().await;
            }
        });
    }
}

fn handle_interrupt() {
    log::warn!(
        "{}",
        TextComponent::text("Received interrupt signal; stopping server...")
            .color_named(NamedColor::Red)
            .to_pretty_console()
    );
    std::process::exit(0);
}

// Non-UNIX Ctrl-C handling
#[cfg(not(unix))]
async fn setup_sighandler() -> io::Result<()> {
    if ctrl_c().await.is_ok() {
        handle_interrupt();
    }

    Ok(())
}

// Unix signal handling
#[cfg(unix)]
async fn setup_sighandler() -> io::Result<()> {
    if signal(SignalKind::interrupt())?.recv().await.is_some() {
        handle_interrupt();
    }

    if signal(SignalKind::hangup())?.recv().await.is_some() {
        handle_interrupt();
    }

    if signal(SignalKind::terminate())?.recv().await.is_some() {
        handle_interrupt();
    }

    Ok(())
}

fn setup_console(server: Arc<Server>) {
    tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut reader = BufReader::new(stdin);
        loop {
            let mut out = String::new();

            reader
                .read_line(&mut out)
                .await
                .expect("Failed to read console line");

            if !out.is_empty() {
                let dispatcher = server.command_dispatcher.clone();
                dispatcher
                    .handle_command(&mut command::CommandSender::Console, &server, &out)
                    .await;
            }
        }
    });
}
