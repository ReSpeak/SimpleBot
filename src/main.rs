use std::fs;
use std::path::PathBuf;

use failure::format_err;
use futures::{Future, Stream};
use futures::sync::oneshot;
use lazy_static::lazy_static;
use regex::Regex;
use serde::Deserialize;
use slog::{debug, error, info, o, warn, Drain, Logger};
use structopt::clap::AppSettings;
use structopt::StructOpt;
use tsclientlib::events::Event;
use tsclientlib::{
	ClientId, ConnectionLock, ConnectOptions, Connection, DisconnectOptions,
	Reason, TextMessageTargetMode,
};

const SETTINGS_FILENAME: &str = "settings.toml";
const PRIVATE_KEY_FILENAME: &str = "private.key";

#[derive(StructOpt, Debug)]
#[structopt(raw(global_settings = "&[AppSettings::ColoredHelp, \
                                   AppSettings::VersionlessSubcommands]"))]
struct Args {
	#[structopt(long = "settings", help = "The path of the settings file")]
	settings: Option<String>,

	#[structopt(
		short = "v",
		long = "verbose",
		help = "Print the content of all packets",
		parse(from_occurrences)
	)]
	verbose: u8,
	// 0. Print nothing
	// 1. Print command string
	// 2. Print packets
	// 3. Print udp packets
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct SettingsFile {
	/// The address of the server to connect to
	///
	/// # Default
	/// `localhost`
	address: Option<String>,
	/// The name of the bot.
	///
	/// # Default
	/// `SimpleBot`
	name: Option<String>,
	/// The disconnect message of the bot.
	///
	/// # Default
	/// `Disconnecting`
	disconnect_message: Option<String>,
}

#[derive(Debug)]
struct Settings {
	address: String,
	name: String,
	disconnect_message: String,
}

fn main() -> Result<(), failure::Error> {
	// Parse command line options
	let args = Args::from_args();

	// Create logger
	let logger = {
		let decorator = slog_term::TermDecorator::new().build();
		let drain = slog_term::CompactFormat::new(decorator).build().fuse();
		let drain = slog_async::Async::new(drain).build().fuse();

		slog::Logger::root(drain, o!())
	};

	// Load settings
	let proj_dirs =
		match directories::ProjectDirs::from("", "ReSpeak", "simple-bot") {
			Some(r) => r,
			None => {
				panic!("Failed to get project directory");
			}
		};
	let file = args
		.settings
		.as_ref()
		.map(PathBuf::from)
		.unwrap_or_else(|| proj_dirs.config_dir().join(SETTINGS_FILENAME));
	let settings = match fs::read_to_string(&file) {
		Ok(r) => toml::from_str(&r).unwrap(),
		Err(e) => {
			info!(logger, "Failed to read settings, using defaults"; "error" => ?e);
			SettingsFile::default()
		}
	};
	let settings = Settings {
		address: settings.address.unwrap_or_else(|| "localhost".parse().unwrap()),
		name: settings.name.unwrap_or_else(|| "SimpleBot".into()),
		disconnect_message: settings.disconnect_message.unwrap_or_else(||
			"Disconnecting".into())
	};

	// Load private key
	let file = file.parent().unwrap().join(PRIVATE_KEY_FILENAME);
	let private_key = match fs::read(&file) {
		Ok(r) => tsproto::crypto::EccKeyPrivP256::import(&r)?,
		_ => {
			// Create new key
			let key = tsproto::crypto::EccKeyPrivP256::create()?;

			// Create directory
			if let Err(e) = fs::create_dir_all(proj_dirs.config_dir()) {
				error!(logger, "Failed to create config dictionary"; "error" => ?e);
			}
			// Write to file
			if let Err(e) = fs::write(&file, &key.to_short()) {
				warn!(logger, "Failed to store the private key, the server \
					identity will not be the same in the next run";
					"file" => ?file.to_str(),
					"error" => ?e);
			}

			key
		}
	};

	let (disconnect_send, disconnect_recv) = oneshot::channel();
	let disconnect_message = settings.disconnect_message.clone();
	let logger2 = logger.clone();
	tokio::run(
		futures::lazy(move || {
			let con_config = ConnectOptions::new(settings.address)
				.private_key(private_key)
				.name(settings.name)
				.logger(logger.clone())
				.log_commands(args.verbose >= 1)
				.log_packets(args.verbose >= 2)
				.log_udp_packets(args.verbose >= 3);

			// Connect
			Connection::new(con_config)
		})
		.and_then(|con| {
			con.add_on_disconnect(Box::new(move || disconnect_send.send(()).unwrap()));
			// Listen to events
			con.add_on_event("listener".into(), Box::new(move |c, e|
				handle_event(&logger2, c, e)));

			Ok(con)
		})
		.and_then(|con| {
			// Wait for ctrl + c
			let ctrl_c = tokio_signal::ctrl_c().flatten_stream();
			ctrl_c
				.into_future()
				.map_err(|_| format_err!("Failed to wait for ctrl + c").into())
				.map(move |_| con)
		})
		.and_then(|con| {
			// Disconnect
			con.disconnect(
				DisconnectOptions::new()
					.reason(Reason::Clientdisconnect)
					.message(disconnect_message),
			)
		})
		.map_err(|e| panic!("An error occurred {:?}", e))
		// Also quit on disconnect event
		.select(disconnect_recv.map_err(|_|
			format_err!("Failed to receive disconnect")))
		.map(|_| ())
		.map_err(|_| panic!("An error occurred"))
	);

	Ok(())
}

fn respond(
	con: &ConnectionLock,
	logger: Logger,
	mode: TextMessageTargetMode,
	to: Option<ClientId>,
	msg: &str,
) {
	let con_mut = con.to_mut();
	match mode {
		TextMessageTargetMode::Client => if let Some(to) = to {
			if let Some(client) = con_mut.get_server().get_client(&to) {
				tokio::spawn(client
					.send_textmessage(msg)
					.map_err(move |e| error!(logger,
						"Failed to send message to channel";
						"error" => ?e)));
			} else {
				error!(logger, "Cannot find client"; "id" => ?to);
			}
		} else {
			error!(logger, "Got message from client but from is not set");
		}
		TextMessageTargetMode::Channel => {
			tokio::spawn(con_mut.send_channel_textmessage(msg)
				.map_err(move |e| error!(logger,
					"Failed to send message to channel";
					"error" => ?e)));
		}
		TextMessageTargetMode::Server => {
			tokio::spawn(con_mut.get_server().send_textmessage(msg)
				.map_err(move |e| error!(logger,
					"Failed to send message to channel";
					"error" => ?e)));
		}
		TextMessageTargetMode::Unknown => {
			error!(logger, "Unknown text message target");
		}
	}
}

fn handle_event(logger: &Logger, con: &ConnectionLock, event: &[Event]) {
	lazy_static! {
		static ref CONTAINS_QUIT: Regex = Regex::new(
			r"(?i)quit|leave|exit|dumb").unwrap();
		static ref CONTAINS_HI: Regex = Regex::new(
			r"(?i)hi|hello").unwrap();
	}

	for e in event {
		match e {
			Event::TextMessage { mode, from, message } => {
				// Ignore messages from ourself
				if from.map(|id| id == con.own_client).unwrap_or_default() {
					continue;
				}

				debug!(logger, "Got message"; "mode" => ?mode,
					"from" => ?from, "message" => message);
				if CONTAINS_QUIT.is_match(message) {
					let logger = logger.clone();
					const QUIT_HELP: &str = "I will leave if you poke me with arrows";
					respond(con, logger.clone(), *mode, *from, QUIT_HELP);
				} else if CONTAINS_HI.is_match(message) {
					if let Some(client) = from.and_then(|id| con.server.clients.get(&id)) {
						let msg = format!("Hi {}, how are you?", client.name);
						respond(con, logger.clone(), *mode, *from, &msg);
					}
				}
			}
			Event::Poke { from, message } => {
				if message.eq_ignore_ascii_case("arrows") {
					info!(logger, "Leaving on request"; "from" => ?from);
					// We get no disconnect message here
					let logger = logger.clone();
					tokio::spawn(con.to_mut().remove()
						.map_err(move |e| error!(logger,
							"Failed to disconnect";
							"error" => ?e)));
				}
			}
			_ => {}
		}
	}
}
