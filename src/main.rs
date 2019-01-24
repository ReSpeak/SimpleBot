use std::fs;
use std::path::PathBuf;
use std::sync::Arc;

use failure::format_err;
use futures::{Future, Stream};
use futures::sync::oneshot;
use lazy_static::lazy_static;
use parking_lot::Mutex;
use regex::Regex;
use serde::Deserialize;
use slog::{debug, error, info, o, warn, Drain, Logger};
use structopt::clap::AppSettings;
use structopt::StructOpt;
use tsclientlib::events::Event;
use tsclientlib::{
	ClientId, ConnectionLock, ConnectOptions, Connection, DisconnectOptions,
	InvokerRef, Reason, TextMessageTargetMode,
};

const SETTINGS_FILENAME: &str = "settings.toml";
/// Dynamically added actions. This file will be overwritten automatically.
const DYNAMIC_FILENAME: &str = "dynamic.toml";
const PRIVATE_KEY_FILENAME: &str = "private.key";

mod action;
mod builtins;

use crate::action::{ActionDefinition, ActionList};

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

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct ActionFile {
	#[serde(default = "Vec::new")]
	on_message: Vec<ActionDefinition>,
	#[serde(default = "Vec::new")]
	on_poke: Vec<ActionDefinition>,
	#[serde(default = "Vec::new")]
	include: Vec<String>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct Settings {
	/// The address of the server to connect to
	///
	/// # Default
	/// `localhost`
	#[serde(default = "default_address")]
	address: String,
	/// The name of the bot.
	///
	/// # Default
	/// `SimpleBot`
	#[serde(default = "default_name")]
	name: String,
	/// The disconnect message of the bot.
	///
	/// # Default
	/// `Disconnecting`
	#[serde(default = "default_disconnect_message")]
	disconnect_message: String,

	/// The prefix for builtin commands.
	///
	/// # Default
	/// `.`
	#[serde(default = "default_prefix")]
	prefix: String,

	#[serde(default = "Default::default")]
	actions: ActionFile,
}

pub struct Bot {
	logger: Logger,
	actions: ActionList,
	settings: Settings,
}

#[derive(Clone, Debug)]
struct Message<'a> {
	/// If this is `None`, it means poke.
	mode: Option<TextMessageTargetMode>,
	invoker: InvokerRef<'a>,
	message: &'a str,
}

impl Default for Settings {
	fn default() -> Self {
		Self {
			address: default_address(),
			name: default_name(),
			disconnect_message: default_disconnect_message(),
			prefix: default_prefix(),

			actions: Default::default(),
		}
	}
}

fn default_address() -> String { "localhost".into() }
fn default_name() -> String { "SimpleBot".into() }
fn default_disconnect_message() -> String { "Disconnecting".into() }
fn default_prefix() -> String { ".".into() }

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
			Settings::default()
		}
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

	let disconnect_message = settings.disconnect_message.clone();
	let con_config = ConnectOptions::new(settings.address.clone())
		.private_key(private_key)
		.name(settings.name.clone())
		.logger(logger.clone())
		.log_commands(args.verbose >= 1)
		.log_packets(args.verbose >= 2)
		.log_udp_packets(args.verbose >= 3);

	let bot = Arc::new(Mutex::new(Bot {
		logger: logger.clone(),
		actions: ActionList::default(),
		settings,
	}));

	let (disconnect_send, disconnect_recv) = oneshot::channel();
	tokio::run(
		futures::lazy(move || {
			// Connect
			Connection::new(con_config)
		})
		.and_then(|con| {
			con.add_on_disconnect(Box::new(move || disconnect_send.send(()).unwrap()));
			// Listen to events
			con.add_on_event("listener".into(), Box::new(move |c, e| {
				let mut bot = bot.lock();
				handle_event(&mut *bot, c, e)
			}));

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

fn handle_event(bot: &mut Bot, con: &ConnectionLock, event: &[Event]) {
	lazy_static! {
		static ref CONTAINS_QUIT: Regex = Regex::new(
			r"(?i)quit|leave|exit|dumb").unwrap();
		static ref CONTAINS_HI: Regex = Regex::new(
			r"(?i)hi|hello").unwrap();
	}

	for e in event {
		match e {
			Event::TextMessage { mode, invoker, message } => {
				// Ignore messages from ourself
				if invoker.id == con.own_client {
					continue;
				}

				debug!(bot.logger, "Got message"; "mode" => ?mode,
					"invoker" => ?invoker, "message" => message);
				if CONTAINS_QUIT.is_match(message) {
					let logger = bot.logger.clone();
					const QUIT_HELP: &str = "I will leave if you poke me with arrows";
					let id = if invoker.id.0 == 0 { None } else { Some(invoker.id) };

					respond(con, logger.clone(), *mode, id, QUIT_HELP);
				}
			}
			Event::Poke { invoker, message } => {
				if invoker.id == con.own_client {
					continue;
				}

				// TODO Poke back with answer

				if message.eq_ignore_ascii_case("arrows") {
					info!(bot.logger, "Leaving on request"; "invoker" => ?invoker);
					// We get no disconnect message here
					let logger = bot.logger.clone();
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
