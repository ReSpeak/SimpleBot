use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Weak};

use failure::{bail, format_err};
use futures::{Future, Stream};
use futures::sync::oneshot;
use lazy_static::lazy_static;
use parking_lot::RwLock;
use serde::{Deserialize, Serialize};
use slog::{debug, error, o, warn, Drain, Logger};
use structopt::clap::AppSettings;
use structopt::StructOpt;
use tsclientlib::events::Event;
use tsclientlib::{
	ClientId, ConnectionLock, ConnectOptions, Connection, DisconnectOptions,
	InvokerRef, Reason, TextMessageTargetMode,
};

const SETTINGS_FILENAME: &str = "settings.toml";

type Result<T> = std::result::Result<T, failure::Error>;

pub mod action;
pub mod builtins;

use crate::action::{ActionDefinition, ActionList};

lazy_static! {
	static ref LOGGER: Logger = {
		let decorator = slog_term::TermDecorator::new().build();
		let drain = slog_term::CompactFormat::new(decorator).build().fuse();
		let drain = slog_async::Async::new(drain).build().fuse();

		Logger::root(drain, o!())
	};
}

#[derive(StructOpt, Debug)]
#[structopt(raw(global_settings = "&[AppSettings::ColoredHelp, \
	AppSettings::VersionlessSubcommands]"))]
struct Args {
	#[structopt(short = "s", long = "settings", help = "The path of the settings file")]
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

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ActionFile {
	/// Includes other files.
	///
	/// The path is always relative to the current file. Includes will be
	/// inserted after the declarations in this file.
	#[serde(default = "Vec::new")]
	include: Vec<String>,

	// This needs to be second for the toml serialization.
	#[serde(default = "Vec::new")]
	on_message: Vec<ActionDefinition>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ChannelDefinition {
	Id(u64),
	Name(String),
}

#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
	/// The file which contains the private key.
	///
	/// This will be automatically generated on the first run.
	///
	/// # Default
	/// `private.key`
	#[serde(default = "default_key_file")]
	key_file: String,
	/// Dynamically added actions. This file will be overwritten automatically.
	///
	/// The actions from this file will be added after the normal actions and
	/// before the builtins.
	///
	/// # Default
	/// `dynamic.toml`
	#[serde(default = "default_dynamic_actions")]
	dynamic_actions: String,

	/// The address of the server to connect to
	///
	/// # Default
	/// `localhost`
	#[serde(default = "default_address")]
	address: String,
	// TODO Support a default channel
	channel: Option<ChannelDefinition>,
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
	base_dir: PathBuf,
	settings_path: PathBuf,
	actions: ActionList,
	settings: Settings,
}

#[derive(Clone, Debug)]
pub struct Message<'a> {
	/// If this is `None`, it means poke.
	mode: Option<TextMessageTargetMode>,
	invoker: InvokerRef<'a>,
	message: &'a str,
}

impl Default for Bot {
	fn default() -> Self {
		Self {
			logger: LOGGER.clone(),
			base_dir: PathBuf::new(),
			settings_path: PathBuf::new(),
			actions: Default::default(),
			settings: Default::default(),
		}
	}
}

impl Default for Settings {
	fn default() -> Self {
		Self {
			key_file: default_key_file(),
			dynamic_actions: default_dynamic_actions(),

			address: default_address(),
			channel: None,
			name: default_name(),
			disconnect_message: default_disconnect_message(),
			prefix: default_prefix(),

			actions: Default::default(),
		}
	}
}

fn default_key_file() -> String { "private.key".into() }

fn default_address() -> String { "localhost".into() }
fn default_name() -> String { "SimpleBot".into() }
fn default_disconnect_message() -> String { "Disconnecting".into() }
fn default_prefix() -> String { ".".into() }
fn default_dynamic_actions() -> String { "dynamic.toml".into() }

fn main() -> Result<()> {
	// Parse command line options
	let args = Args::from_args();

	let logger = LOGGER.clone();

	// Load settings
	let settings_path;
	let base_dir;
	if let Some(settings) = &args.settings {
		settings_path = PathBuf::from(settings.to_string());
		base_dir = settings_path.parent().map(|p| p.into()).unwrap_or_else(PathBuf::new);
	} else {
		let proj_dirs =
			match directories::ProjectDirs::from("", "ReSpeak", "simple-bot") {
				Some(r) => r,
				None => {
					panic!("Failed to get project directory");
				}
			};
		base_dir = proj_dirs.config_dir().into();
		settings_path = base_dir.join(SETTINGS_FILENAME);
	}

	let mut bot = Bot::default();
	bot.base_dir = base_dir;
	bot.settings_path = settings_path;
	let bot = Arc::new(RwLock::new(bot));
	let private_key;
	let disconnect_message;
	let con_config;
	{
		let mut b = bot.write();
		load_settings(Arc::downgrade(&bot), &mut *b)?;
		disconnect_message = b.settings.disconnect_message.clone();

		// Load private key
		let file = Path::new(&b.settings.key_file);
		let file = if file.is_absolute() {
			file.to_path_buf()
		} else {
			b.base_dir.join(&b.settings.key_file)
		};
		private_key = match fs::read(&file) {
			Ok(r) => tsproto::crypto::EccKeyPrivP256::import(&r)?,
			_ => {
				// Create new key
				let key = tsproto::crypto::EccKeyPrivP256::create()?;

				// Create directory
				if let Err(e) = fs::create_dir_all(&b.base_dir) {
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

		con_config = ConnectOptions::new(b.settings.address.clone())
			.private_key(private_key)
			.name(b.settings.name.clone())
			.logger(logger.clone())
			.log_commands(args.verbose >= 1)
			.log_packets(args.verbose >= 2)
			.log_udp_packets(args.verbose >= 3);
	}

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
				let bot = bot.read();
				handle_event(&*bot, c, e)
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

fn load_settings(b2: Weak<RwLock<Bot>>, bot: &mut Bot) -> Result<()> {
	// Reload settings
	match fs::read_to_string(&bot.settings_path) {
		Ok(r) => {
			match toml::from_str(&r) {
				Ok(s) => bot.settings = s,
				Err(e) => bail!("Failed to parse settings: {}", e),
			}
		}
		Err(e) => {
			// Only a soft error
			warn!(bot.logger, "Failed to read settings, using defaults";
				"error" => ?e);
		}
	}

	// Reload actions
	let mut actions = ActionList::default();
	if let Err(e) = crate::load_actions(&bot.base_dir, &mut actions,
		&bot.settings.actions) {
		error!(bot.logger, "Failed to load actions"; "error" => %e);
	}

	// Load builtins here, otherwise .del will never trigger
	builtins::init(b2, bot);

	// Dynamic actions
	let path = Path::new(&bot.settings.dynamic_actions);
	let path = if path.is_absolute() {
		path.into()
	} else {
		bot.base_dir.join(path)
	};
	let dynamic: ActionFile = match fs::read_to_string(&path) {
		Ok(s) => toml::from_str(&s)?,
		Err(e) => {
			debug!(bot.logger, "Dynamic actions not loaded"; "error" => %e);
			ActionFile::default()
		}
	};
	if let Err(e) = crate::load_actions(&bot.base_dir, &mut actions,
		&dynamic) {
		bail!("Failed to load dynamic actions: {}", e);
	}
	bot.actions = actions;
	Ok(())
}

fn load_actions(base: &Path, actions: &mut ActionList, f: &ActionFile) -> Result<()> {
	for a in &f.on_message {
		actions.0.push(a.to_action()?);
	}
	// Handle includes
	for i in &f.include {
		let path = base.join(i);
		let f2: ActionFile = toml::from_str(&fs::read_to_string(&path)?)?;
		load_actions(path.parent().unwrap_or(base), actions, &f2)?;
	}

	Ok(())
}

fn respond(
	con: &ConnectionLock,
	logger: Logger,
	mode: Option<TextMessageTargetMode>,
	to: Option<ClientId>,
	msg: &str,
) {
	debug!(logger, "Answering"; "message" => msg, "to" => ?to, "mode" => ?mode);
	let con_mut = con.to_mut();
	match mode {
		Some(TextMessageTargetMode::Client) => if let Some(to) = to {
			if let Some(client) = con_mut.get_server().get_client(&to) {
				tokio::spawn(client
					.send_textmessage(msg)
					.map_err(move |e| error!(logger,
						"Failed to send message to channel";
						"error" => ?e)));
			} else {
				warn!(logger, "Failed to answer client: Not in \
					view (this may be fixed later)");
			}
		} else {
			error!(logger, "Got message from client but from is not set");
		}
		Some(TextMessageTargetMode::Channel) => {
			tokio::spawn(con_mut.send_channel_textmessage(msg)
				.map_err(move |e| error!(logger,
					"Failed to send message to channel";
					"error" => ?e)));
		}
		Some(TextMessageTargetMode::Server) => {
			tokio::spawn(con_mut.get_server().send_textmessage(msg)
				.map_err(move |e| error!(logger,
					"Failed to send message to channel";
					"error" => ?e)));
		}
		Some(TextMessageTargetMode::Unknown) => {
			error!(logger, "Unknown text message target");
		}
		// Poke
		None => if let Some(to) = to {
			// Try to find client
			if let Some(client) = con.to_mut().get_server().get_client(&to) {
				tokio::spawn(client.poke(msg)
					.map_err(move |e| error!(logger,
						"Failed to poke client"; "error" => ?e)));
			} else {
				warn!(logger, "Failed to poke back client: Not in \
					view (this may be fixed later)");
			}
		} else {
			error!(logger, "Got poke from client but from is not set");
		}
	}
}

fn handle_event(bot: &Bot, con: &ConnectionLock, event: &[Event]) {
	for e in event {
		match e {
			Event::TextMessage { mode, invoker, message } => {
				// Ignore messages from ourself
				if invoker.id == con.own_client {
					continue;
				}

				debug!(bot.logger, "Got message"; "mode" => ?mode,
					"invoker" => ?invoker, "message" => message);

				let msg = Message {
					mode: Some(*mode),
					invoker: invoker.as_ref(),
					message,
				};
				if let Some(response) = bot.actions.handle(con, &msg) {
					respond(con, bot.logger.clone(), Some(*mode), Some(invoker.id), response.as_ref());
				}
			}
			Event::Poke { invoker, message } => {
				if invoker.id == con.own_client {
					continue;
				}

				debug!(bot.logger, "Got poked"; "invoker" => ?invoker,
				   "message" => message);

				let msg = Message {
					mode: None,
					invoker: invoker.as_ref(),
					message,
				};
				if let Some(response) = bot.actions.handle(con, &msg) {
					respond(con, bot.logger.clone(), None, Some(invoker.id), response.as_ref());
				}
			}
			_ => {}
		}
	}
}
