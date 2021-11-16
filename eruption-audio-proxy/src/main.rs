/*
    This file is part of Eruption.

    Eruption is free software: you can redistribute it and/or modify
    it under the terms of the GNU General Public License as published by
    the Free Software Foundation, either version 3 of the License, or
    (at your option) any later version.

    Eruption is distributed in the hope that it will be useful,
    but WITHOUT ANY WARRANTY; without even the implied warranty of
    MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
    GNU General Public License for more details.

    You should have received a copy of the GNU General Public License
    along with Eruption.  If not, see <http://www.gnu.org/licenses/>.
*/

use std::io::Cursor;
use std::mem::MaybeUninit;
use std::os::unix::io::AsRawFd;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use std::{env, thread};

use clap::{IntoApp, Parser};
use clap_generate::Shell;
use crossbeam::channel::{unbounded, Receiver};
use i18n_embed::{
    fluent::{fluent_language_loader, FluentLanguageLoader},
    DesktopLanguageRequester,
};
use lazy_static::lazy_static;
use log::{debug, error, info, trace, warn};
use nix::poll::{poll, PollFd, PollFlags};
use parking_lot::Mutex;
use prost::Message;
use rust_embed::RustEmbed;
use socket2::{Domain, SockAddr, Socket, Type};
use syslog::Facility;
use tokio::io::{self};

use protocol::Command;
use protocol::CommandType;

use crate::audio::AudioBackend;

mod audio;
mod constants;
mod util;

#[derive(RustEmbed)]
#[folder = "i18n"] // path to the compiled localization resources
struct Localizations;

lazy_static! {
    /// Global configuration
    pub static ref STATIC_LOADER: Arc<Mutex<Option<FluentLanguageLoader>>> = Arc::new(Mutex::new(None));

    pub static ref RECORDING: AtomicBool = AtomicBool::new(false);

    /// A queue of packets that will be send to the Eruption daemon
    pub static ref PACKET_TX_QUEUE: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(Vec::new()));
}

#[allow(unused)]
macro_rules! tr {
    ($message_id:literal) => {{
        let loader = $crate::STATIC_LOADER.lock();
        let loader = loader.as_ref().unwrap();

        i18n_embed_fl::fl!(loader, $message_id)
    }};

    ($message_id:literal, $($args:expr),*) => {{
        let loader = $crate::STATIC_LOADER.lock();
        let loader = loader.as_ref().unwrap();

        i18n_embed_fl::fl!(loader, $message_id, $($args), *)
    }};
}

pub mod protocol {
    include!(concat!(env!("OUT_DIR"), "/audio_proxy.rs"));
}

type Result<T> = std::result::Result<T, eyre::Error>;

lazy_static! {
    // /// Global command line options
    // pub static ref OPTIONS: Arc<Mutex<Option<Options>>> = Arc::new(Mutex::new(None));

    pub static ref AUDIO_BACKEND: Arc<Mutex<audio::PulseAudioBackend>> =  Arc::new(Mutex::new(audio::PulseAudioBackend::new()));


    /// Global "quit" status flag
    pub static ref QUIT: AtomicBool = AtomicBool::new(false);
}

#[derive(Debug, thiserror::Error)]
pub enum MainError {
    #[error("Could not parse syslog log-level")]
    SyslogLevelError {},

    #[error("Unknown error: {description}")]
    UnknownError { description: String },
}

lazy_static! {
    static ref ABOUT: String = tr!("about");
    static ref VERBOSE_ABOUT: String = tr!("verbose-about");
    static ref CONFIG_ABOUT: String = tr!("config-about");
    static ref DAEMON_ABOUT: String = tr!("daemon-about");
    static ref COMPLETIONS_ABOUT: String = tr!("completions-about");
}

/// Supported command line arguments
#[derive(Debug, clap::Parser)]
#[clap(
    version = env ! ("CARGO_PKG_VERSION"),
    author = "X3n0m0rph59 <x3n0m0rph59@gmail.com>",
    about = ABOUT.as_str(),
)]
pub struct Options {
    #[clap(
        about(VERBOSE_ABOUT.as_str()),
        short,
        long,
        parse(from_occurrences)
    )]
    verbose: u8,

    #[clap(about(CONFIG_ABOUT.as_str()), short, long)]
    config: Option<String>,

    #[clap(subcommand)]
    command: Subcommands,
}

// Sub-commands
#[derive(Debug, clap::Parser)]
pub enum Subcommands {
    #[clap(about(DAEMON_ABOUT.as_str()))]
    Daemon,

    #[clap(about(COMPLETIONS_ABOUT.as_str()))]
    Completions {
        // #[clap(subcommand)]
        shell: Shell,
    },
}

/// Print license information
#[allow(dead_code)]
fn print_header() {
    println!("{}", tr!("license-header"));
    println!();
}

pub async fn run_main_loop(_ctrl_c_rx: &Receiver<bool>) -> Result<()> {
    unsafe fn assume_init(buf: &[MaybeUninit<u8>]) -> &[u8] {
        &*(buf as *const [MaybeUninit<u8>] as *const [u8])
    }

    debug!("Entering the main loop now...");

    'MAIN_LOOP: loop {
        if QUIT.load(Ordering::SeqCst) {
            break 'MAIN_LOOP Ok(());
        }

        debug!("Connecting to the Eruption audio proxy socket...");

        let socket = Socket::new(Domain::UNIX, Type::SEQPACKET, None)?;
        let address = SockAddr::unix(&constants::AUDIO_SOCKET_NAME)?;

        match socket.connect(&address) {
            Ok(()) => {
                info!("Connected to Eruption daemon");

                // socket.set_nodelay(true)?;
                socket.set_send_buffer_size(constants::NET_BUFFER_CAPACITY * 2)?;
                socket.set_recv_buffer_size(constants::NET_BUFFER_CAPACITY * 2)?;

                let mut last_status_update = Instant::now();

                'EVENT_LOOP: loop {
                    if QUIT.load(Ordering::SeqCst) {
                        break 'MAIN_LOOP Ok(());
                    }

                    // record samples to the global sample buffer
                    if RECORDING.load(Ordering::SeqCst) {
                        let mut audio_backend = AUDIO_BACKEND.lock();
                        if let Err(e) = audio_backend.record_samples() {
                            error!("An error occurred while recording audio: {}", e);

                            // sleep a while then re-open audio devices
                            thread::sleep(Duration::from_millis(constants::SLEEP_TIME_TIMEOUT));

                            debug!("Re-opening audio device");
                            audio_backend.open()?;
                        }
                    }

                    // wait for socket to be ready
                    let mut poll_fds = [PollFd::new(
                        socket.as_raw_fd(),
                        PollFlags::POLLIN
                            | PollFlags::POLLOUT
                            | PollFlags::POLLHUP
                            | PollFlags::POLLERR,
                    )];

                    let result = poll(&mut poll_fds, constants::SLEEP_TIME_TIMEOUT as i32)?;

                    if poll_fds[0].revents().unwrap().contains(PollFlags::POLLHUP)
                        | poll_fds[0].revents().unwrap().contains(PollFlags::POLLERR)
                    {
                        warn!("Socket error: Eruption disconnected");

                        break 'EVENT_LOOP;
                    }

                    if result > 0 {
                        if poll_fds[0].revents().unwrap().contains(PollFlags::POLLIN) {
                            trace!("Receiving a protocol packet...");

                            // read data
                            let mut tmp = [MaybeUninit::zeroed(); constants::NET_BUFFER_CAPACITY];
                            match socket.recv(&mut tmp) {
                                Ok(0) => {
                                    info!("Eruption daemon disconnected");

                                    break 'EVENT_LOOP;
                                }

                                Ok(_n) => {
                                    let buf = unsafe { assume_init(&tmp[..tmp.len()]) };
                                    match Command::decode_length_delimited(&mut Cursor::new(buf)) {
                                        Ok(message) => {
                                            let mut response = protocol::Response::default();

                                            match message.command_type() {
                                                CommandType::StartRecording => {
                                                    info!("Opening audio device");

                                                    let mut audio_backend = AUDIO_BACKEND.lock();
                                                    audio_backend.open()?;

                                                    RECORDING.store(true, Ordering::SeqCst);

                                                    response.set_response_type(CommandType::Noop);
                                                }

                                                CommandType::StopRecording => {
                                                    info!("Closing audio device");

                                                    let mut audio_backend = AUDIO_BACKEND.lock();
                                                    audio_backend.close()?;

                                                    RECORDING.store(false, Ordering::SeqCst);

                                                    response.set_response_type(CommandType::Noop);
                                                }

                                                CommandType::AudioVolume => {
                                                    trace!("Request for audio volume");

                                                    let audio_backend = AUDIO_BACKEND.lock();
                                                    let volume =
                                                        audio_backend.get_audio_volume()?;

                                                    response.set_response_type(
                                                        CommandType::AudioVolume,
                                                    );
                                                    response.payload = Some(
                                                        protocol::response::Payload::Volume(volume),
                                                    );
                                                }

                                                CommandType::AudioMutedState => {
                                                    trace!("Request for audio muted state");

                                                    let audio_backend = AUDIO_BACKEND.lock();
                                                    let muted = audio_backend.is_audio_muted()?;

                                                    response.set_response_type(
                                                        CommandType::AudioMutedState,
                                                    );
                                                    response.payload = Some(
                                                        protocol::response::Payload::Muted(muted),
                                                    );
                                                }

                                                _ => {
                                                    error!("Protocol error: Unknown command");
                                                }
                                            }

                                            let mut buf = Vec::new();
                                            response.encode_length_delimited(&mut buf)?;

                                            // enqueue the response packet
                                            PACKET_TX_QUEUE.lock().push(buf);
                                        }

                                        Err(e) => {
                                            error!("Protocol error: {}", e);
                                        }
                                    }
                                }

                                Err(e) => {
                                    error!(
                                        "Error occurred during receive from audio proxy socket: {}",
                                        e
                                    );
                                }
                            }
                        }

                        if poll_fds[0].revents().unwrap().contains(PollFlags::POLLOUT) {
                            if RECORDING.load(Ordering::SeqCst) {
                                let samples = audio::AUDIO_BUFFER.read().clone();

                                let mut response = protocol::Response::default();

                                response.set_response_type(CommandType::AudioData);
                                response.payload = Some(protocol::response::Payload::Data(samples));

                                let mut buf = Vec::new();
                                response.encode_length_delimited(&mut buf)?;

                                PACKET_TX_QUEUE.lock().push(buf);
                            }

                            // send unsolicited audio state updates every n milliseconds
                            if last_status_update.elapsed() >= Duration::from_millis(100) {
                                let audio_backend = AUDIO_BACKEND.lock();

                                // audio volume
                                let volume = audio_backend.get_audio_volume()?;

                                let mut response = protocol::Response::default();
                                response.set_response_type(CommandType::AudioVolume);
                                response.payload =
                                    Some(protocol::response::Payload::Volume(volume));

                                let mut buf = Vec::new();
                                response.encode_length_delimited(&mut buf)?;

                                PACKET_TX_QUEUE.lock().push(buf);

                                // audio muted state
                                let muted = audio_backend.is_audio_muted()?;

                                let mut response = protocol::Response::default();

                                response.set_response_type(CommandType::AudioMutedState);
                                response.payload = Some(protocol::response::Payload::Muted(muted));

                                let mut buf = Vec::new();
                                response.encode_length_delimited(&mut buf)?;

                                PACKET_TX_QUEUE.lock().push(buf);

                                last_status_update = Instant::now();
                            }

                            // transmit the queue of packets to the Eruption daemon
                            while let Some(buf) = PACKET_TX_QUEUE.lock().pop() {
                                trace!("Sending a protocol packet...");

                                // send data
                                match socket.send(&buf) {
                                    Ok(n) => {
                                        if QUIT.load(Ordering::SeqCst) {
                                            break 'MAIN_LOOP Ok(());
                                        }

                                        trace!("Wrote {} bytes to audio proxy socket", n);
                                    }

                                    Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => {
                                        if QUIT.load(Ordering::SeqCst) {
                                            break 'MAIN_LOOP Ok(());
                                        }

                                        // not an error, so continue
                                        continue 'EVENT_LOOP;
                                    }

                                    Err(e) => {
                                        error!("An error occurred during socket write: {}", e);

                                        break 'EVENT_LOOP;
                                    }
                                }
                            }
                        }
                    }

                    if RECORDING.load(Ordering::SeqCst) {
                        thread::sleep(Duration::from_millis(1));
                    } else {
                        thread::sleep(Duration::from_millis(25));
                    }
                }
            }

            Err(e)
                if e.kind() == io::ErrorKind::NotFound
                    || e.kind() == io::ErrorKind::ConnectionRefused =>
            {
                debug!("Audio proxy socket is currently not available, sleeping now...");

                if QUIT.load(Ordering::SeqCst) {
                    break 'MAIN_LOOP Ok(());
                }

                thread::sleep(Duration::from_millis(
                    constants::SLEEP_TIME_WHILE_DISCONNECTED,
                ));
            }

            Err(e) => {
                error!(
                    "An unknown error occurred while connecting to audio proxy socket: {}",
                    e
                );

                if QUIT.load(Ordering::SeqCst) {
                    break 'MAIN_LOOP Ok(());
                }

                thread::sleep(Duration::from_millis(constants::SLEEP_TIME_TIMEOUT));
            }
        }
    }
}

pub async fn async_main() -> std::result::Result<(), eyre::Error> {
    cfg_if::cfg_if! {
        if #[cfg(debug_assertions)] {
            color_eyre::config::HookBuilder::default()
            .panic_section("Please consider reporting a bug at https://github.com/X3n0m0rph59/eruption")
            .install()?;
        } else {
            color_eyre::config::HookBuilder::default()
            .panic_section("Please consider reporting a bug at https://github.com/X3n0m0rph59/eruption")
            .display_env_section(false)
            .install()?;
        }
    }

    // if unsafe { libc::isatty(0) != 0 } {
    //     print_header();
    // }

    let opts = Options::parse();
    let daemon = matches!(opts.command, Subcommands::Daemon);

    if unsafe { libc::isatty(0) != 0 } && daemon {
        // initialize logging on console
        if env::var("RUST_LOG").is_err() {
            env::set_var("RUST_LOG_OVERRIDE", "info");
            pretty_env_logger::init_custom_env("RUST_LOG_OVERRIDE");
        } else {
            pretty_env_logger::init();
        }
    } else {
        // initialize logging to syslog
        let mut errors_present = false;

        let level_filter = match env::var("RUST_LOG")
            .unwrap_or_else(|_| "info".to_string())
            .to_lowercase()
            .as_str()
        {
            "off" => log::LevelFilter::Off,
            "error" => log::LevelFilter::Error,
            "warn" => log::LevelFilter::Warn,
            "info" => log::LevelFilter::Info,
            "debug" => log::LevelFilter::Debug,
            "trace" => log::LevelFilter::Trace,

            _ => {
                errors_present = true;
                log::LevelFilter::Info
            }
        };

        syslog::init(
            Facility::LOG_USER,
            level_filter,
            Some(env!("CARGO_PKG_NAME")),
        )
        .map_err(|_e| MainError::SyslogLevelError {})?;

        if errors_present {
            log::error!("Could not parse syslog log-level");
        }
    }

    match opts.command {
        Subcommands::Daemon => {
            info!("Starting up...");

            // register ctrl-c handler
            let (ctrl_c_tx, ctrl_c_rx) = unbounded();
            ctrlc::set_handler(move || {
                QUIT.store(true, Ordering::SeqCst);

                ctrl_c_tx
                    .send(true)
                    .unwrap_or_else(|e| error!("Could not send on a channel: {}", e));
            })
            .unwrap_or_else(|e| error!("Could not set CTRL-C handler: {}", e));

            info!("Startup completed");

            // enter the main loop
            run_main_loop(&ctrl_c_rx)
                .await
                .unwrap_or_else(|e| error!("{}", e));

            debug!("Left the main loop");

            info!("Exiting now");
        }

        Subcommands::Completions { shell } => {
            const BIN_NAME: &str = env!("CARGO_PKG_NAME");

            let mut app = Options::into_app();
            let mut fd = std::io::stdout();

            clap_generate::generate(shell, &mut app, BIN_NAME.to_string(), &mut fd);
        }
    };

    Ok(())
}

/// Main program entrypoint
pub fn main() -> std::result::Result<(), eyre::Error> {
    let language_loader: FluentLanguageLoader = fluent_language_loader!();

    let requested_languages = DesktopLanguageRequester::requested_languages();
    i18n_embed::select(&language_loader, &Localizations, &requested_languages)?;

    STATIC_LOADER.lock().replace(language_loader);

    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    runtime.block_on(async move { async_main().await })
}
