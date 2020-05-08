use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use futures::prelude::*;
use serde::{Deserialize, Serialize};
use slog::{debug, error, info, o, warn, Drain, Logger};
use structopt::StructOpt;
use tsclientlib::events::Event;
use tsclientlib::{
	facades, ConnectOptions, Connection, DisconnectOptions, Identity, InvokerRef,
	MessageTarget, Reason, StreamItem,
};

const SETTINGS_FILENAME: &str = "settings.toml";

pub mod action;
pub mod builtins;

use crate::action::{ActionDefinition, ActionList};

#[derive(StructOpt, Debug)]
struct Args {
	/// The path of the settings file.
	#[structopt(short, long)]
	settings: Option<String>,

	/// Print the content of all packets.
	#[structopt(short, long, parse(from_occurrences))]
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
	/// How many messages can be sent per second.
	///
	/// If this limit is exceeded, incoming messages will be ignored.
	///
	/// # Default
	/// `2`
	#[serde(default = "default_rate_limit")]
	rate_limit: u8,

	/// The prefix for builtin commands.
	///
	/// # Default
	/// `.`
	#[serde(default = "default_prefix")]
	prefix: String,

	#[serde(default = "Default::default")]
	actions: ActionFile,
}

#[derive(Debug)]
pub struct Bot {
	logger: Logger,
	base_dir: PathBuf,
	settings_path: PathBuf,
	actions: ActionList,
	settings: Settings,
	rate_limiting: Vec<Instant>,
	/// A cached list of actions
	list: Vec<String>,
	should_reload: Cell<bool>,
}

#[derive(Clone, Debug)]
pub struct Message<'a> {
	target: MessageTarget,
	invoker: InvokerRef<'a>,
	message: &'a str,
}

impl Bot {
	fn new(logger: Logger) -> Self {
		Self {
			logger,
			base_dir: PathBuf::new(),
			settings_path: PathBuf::new(),
			actions: Default::default(),
			settings: Default::default(),
			rate_limiting: Default::default(),
			list: Default::default(),
			should_reload: Default::default(),
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
			rate_limit: default_rate_limit(),
			prefix: default_prefix(),

			actions: Default::default(),
		}
	}
}

fn default_key_file() -> String { "private.key".into() }

fn default_address() -> String { "localhost".into() }
fn default_name() -> String { "SimpleBot".into() }
fn default_disconnect_message() -> String { "Disconnecting".into() }
fn default_rate_limit() -> u8 { 2 }
fn default_prefix() -> String { ".".into() }
fn default_dynamic_actions() -> String { "dynamic.toml".into() }

#[tokio::main]
async fn main() -> Result<()> { real_main().await }

async fn real_main() -> Result<()> {
	let logger = {
		let decorator = slog_term::TermDecorator::new().build();
		let drain = slog_term::CompactFormat::new(decorator).build();
		let drain = slog_envlogger::new(drain).fuse();
		let drain = slog_async::Async::new(drain).build().fuse();

		slog::Logger::root(drain, o!())
	};

	// Parse command line options
	let args = Args::from_args();

	// Load settings
	let settings_path;
	let base_dir;
	if let Some(settings) = &args.settings {
		settings_path = PathBuf::from(settings.to_string());
		base_dir = settings_path
			.parent()
			.map(|p| p.into())
			.unwrap_or_else(PathBuf::new);
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

	let mut bot = Bot::new(logger.clone());
	bot.base_dir = base_dir;
	bot.settings_path = settings_path;
	let private_key;
	let disconnect_message;
	let con_config;
	load_settings(&mut bot)?;
	disconnect_message = bot.settings.disconnect_message.clone();

	// Load private key
	let file = Path::new(&bot.settings.key_file);
	let file = if file.is_absolute() {
		file.to_path_buf()
	} else {
		bot.base_dir.join(&bot.settings.key_file)
	};
	private_key = match fs::read(&file) {
		Ok(r) => tsproto_types::crypto::EccKeyPrivP256::import(&r)?,
		_ => {
			// Create new key
			let key = tsproto_types::crypto::EccKeyPrivP256::create()?;

			// Create directory
			if let Err(e) = fs::create_dir_all(&bot.base_dir) {
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
	let identity = Identity::new(private_key, 0);

	con_config = ConnectOptions::new(bot.settings.address.clone())
		.identity(identity)
		.name(bot.settings.name.clone())
		.logger(logger.clone())
		.log_commands(args.verbose >= 1)
		.log_packets(args.verbose >= 2)
		.log_udp_packets(args.verbose >= 3);

	// Connect
	let mut con= Connection::new(con_config)?;
	let r = con
		.events()
		.try_filter(|e| future::ready(matches!(e, StreamItem::ConEvents(_))))
		.next()
		.await;
	if let Some(r) = r {
		r?;
	}

	loop {
		let mut events = con.events();
		tokio::select! {
			// Wait for ctrl + c
			_ = tokio::signal::ctrl_c() => { break; }
			// Listen to events
			e = events.next() => {
				drop(events);
				if let Some(e) = e {
					if let StreamItem::ConEvents(e) = e? {
						let mut state = con.get_mut_state()?;
						handle_event(&mut bot, &mut state, &e);
						if bot.should_reload.get() {
							bot.should_reload.set(false);
							match load_settings(&mut bot) {
								Ok(()) => info!(bot.logger, "Reloaded successfully"),
								Err(e) => error!(bot.logger, "Failed to reload"; "error" => ?e),
							}
						}
					}
				} else {
					break;
				}
			}
		}
	}

	// Disconnect
	con.disconnect(
		DisconnectOptions::new()
			.reason(Reason::Clientdisconnect)
			.message(disconnect_message),
	)?;
	con.events().for_each(|_| future::ready(())).await;

	Ok(())
}

fn load_settings(bot: &mut Bot) -> Result<()> {
	// Reload settings
	match fs::read_to_string(&bot.settings_path) {
		Ok(r) => match toml::from_str(&r) {
			Ok(s) => bot.settings = s,
			Err(e) => bail!("Failed to parse settings: {}", e),
		},
		Err(e) => {
			// Only a soft error
			warn!(bot.logger, "Failed to read settings, using defaults";
				"error" => ?e);
		}
	}

	// Reload actions
	let mut actions = ActionList::default();
	if let Err(e) =
		load_actions(&bot.base_dir, &mut actions, &bot.settings.actions)
	{
		error!(bot.logger, "Failed to load actions"; "error" => %e);
	}

	// Load builtins here, otherwise .del will never trigger
	bot.actions = actions;
	builtins::init(bot);

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
	if let Err(e) = load_actions(&bot.base_dir, &mut bot.actions, &dynamic) {
		bail!("Failed to load dynamic actions: {}", e);
	}
	builtins::init_list(bot);
	debug!(bot.logger, "Loaded actions"; "actions" => ?bot.actions);
	Ok(())
}

fn load_actions(
	base: &Path,
	actions: &mut ActionList,
	f: &ActionFile,
) -> Result<()>
{
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

fn handle_event(bot: &mut Bot, con: &mut facades::ConnectionMut, event: &[Event]) {
	for e in event {
		match e {
			Event::Message {
				target,
				invoker,
				message,
			} => {
				// Ignore messages from ourself
				if invoker.id == con.own_client {
					continue;
				}
				// Check rate limiting
				{
					let rate = &mut bot.rate_limiting;
					let now = Instant::now();
					let second = Duration::from_secs(1);
					rate.retain(|i| now.duration_since(*i) <= second);
					if rate.len() >= bot.settings.rate_limit as usize {
						warn!(bot.logger, "Ignored message because of rate \
							limiting";
							"target" => ?target,
							"invoker" => ?invoker,
							"message" => message,
						);
						continue;
					}
				}

				debug!(bot.logger, "Got message"; "target" => ?target,
					"invoker" => ?invoker, "message" => message);

				let msg = Message {
					target: *target,
					invoker: invoker.as_ref(),
					message,
				};
				if let Some(response) = bot.actions.handle(bot, con, &msg) {
					bot.rate_limiting.push(Instant::now());
					if let Err(e) = con.send_message(*target, response.as_ref()) {
						error!(bot.logger, "Failed to send response"; "error" => ?e)
					}
				}
			}
			_ => {}
		}
	}
}

fn escape_bb(s: &str) -> String { s.replace('[', "\\[") }
