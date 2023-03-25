use std::cell::Cell;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use clap::Parser;
use futures::prelude::*;
use serde::{Deserialize, Serialize};
use tracing::{debug, error, info, warn};
use tsclientlib::events::Event;
use tsclientlib::{
	ChannelId, Connection, DisconnectOptions, Identity, InvokerRef,
	MessageTarget, OutCommandExt, Reason, StreamItem,
};

const SETTINGS_FILENAME: &str = "settings.toml";

pub mod action;
pub mod builtins;

use crate::action::{ActionDefinition, ActionList};

#[derive(Parser, Debug)]
#[clap(author, version, about)]
struct Args {
	/// The path of the settings file.
	#[clap(short, long)]
	settings: Option<String>,

	/// Print the content of all packets.
	#[clap(short, long, action = clap::ArgAction::Count)]
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

	/// The address of the server to connect to.
	///
	/// # Default
	/// `localhost`
	#[serde(default = "default_address")]
	address: String,
	/// The channel on the server to connect to.
	///
	/// E.g. 4, "My Channel" or "My Channel/Nested"
	///
	/// # Default
	/// `None`
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
	fn new() -> Self {
		Self {
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
	tracing_subscriber::fmt::init();
	// Parse command line options
	let args = Args::parse();

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
		let proj_dirs = match directories_next::ProjectDirs::from(
			"",
			"ReSpeak",
			"simple-bot",
		) {
			Some(r) => r,
			None => {
				panic!("Failed to get project directory");
			}
		};
		base_dir = proj_dirs.config_dir().into();
		settings_path = base_dir.join(SETTINGS_FILENAME);
	}

	let mut bot = Bot::new();
	bot.base_dir = base_dir;
	bot.settings_path = settings_path;
	load_settings(&mut bot)?;
	let disconnect_message = bot.settings.disconnect_message.clone();

	// Load private key
	let file = Path::new(&bot.settings.key_file);
	let file = if file.is_absolute() {
		file.to_path_buf()
	} else {
		bot.base_dir.join(&bot.settings.key_file)
	};
	let private_key = match fs::read(&file) {
		Ok(r) => tsproto_types::crypto::EccKeyPrivP256::import(&r)?,
		_ => {
			// Create new key
			let key = tsproto_types::crypto::EccKeyPrivP256::create();

			// Create directory
			if let Err(error) = fs::create_dir_all(&bot.base_dir) {
				error!(%error, "Failed to create config dictionary");
			}
			// Write to file
			if let Err(error) = fs::write(&file, key.to_short()) {
				warn!(%error, "file" = ?file.to_str(), "Failed to store the private key, the server \
					identity will not be the same in the next run");
			}

			key
		}
	};
	let identity = Identity::new(private_key, 0);

	let mut con_config = Connection::build(bot.settings.address.clone())
		.identity(identity)
		.name(bot.settings.name.clone())
		.log_commands(args.verbose >= 1)
		.log_packets(args.verbose >= 2)
		.log_udp_packets(args.verbose >= 3);

	match &bot.settings.channel {
		Some(ChannelDefinition::Id(channel)) => {
			con_config = con_config.channel_id(ChannelId(*channel));
		}
		Some(ChannelDefinition::Name(channel)) => {
			con_config = con_config.channel(channel.clone());
		}
		_ => {}
	}

	// Connect
	let mut con = con_config.connect()?;
	let r = con
		.events()
		.try_filter(|e| future::ready(matches!(e, StreamItem::BookEvents(_))))
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
					if let StreamItem::BookEvents(e) = e? {
						handle_event(&mut bot, &mut con, &e);
						if bot.should_reload.get() {
							bot.should_reload.set(false);
							match load_settings(&mut bot) {
								Ok(()) => info!("Reloaded successfully"),
								Err(error) => error!(%error, "Failed to reload"),
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
		Err(error) => {
			// Only a soft error
			warn!(%error, "Failed to read settings, using defaults");
		}
	}

	// Reload actions
	let mut actions = ActionList::default();
	if let Err(error) =
		load_actions(&bot.base_dir, &mut actions, &bot.settings.actions)
	{
		error!(%error, "Failed to load actions");
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
	let dynamic: ActionFile = match fs::read_to_string(path) {
		Ok(s) => toml::from_str(&s)?,
		Err(error) => {
			debug!(%error, "Dynamic actions not loaded");
			ActionFile::default()
		}
	};
	if let Err(e) = load_actions(&bot.base_dir, &mut bot.actions, &dynamic) {
		bail!("Failed to load dynamic actions: {}", e);
	}
	builtins::init_list(bot);
	debug!(actions = ?bot.actions, "Loaded actions");
	Ok(())
}

fn load_actions(
	base: &Path,
	actions: &mut ActionList,
	f: &ActionFile,
) -> Result<()> {
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

fn handle_event(bot: &mut Bot, con: &mut Connection, event: &[Event]) {
	for e in event {
		if let Event::Message {
			target,
			invoker,
			message,
		} = e
		{
			// Ignore messages from ourself
			if invoker.id == con.get_state().unwrap().own_client {
				continue;
			}
			// Check rate limiting
			{
				let rate = &mut bot.rate_limiting;
				let now = Instant::now();
				let second = Duration::from_secs(1);
				rate.retain(|i| now.duration_since(*i) <= second);
				if rate.len() >= bot.settings.rate_limit as usize {
					warn!(
						?target,
						?invoker,
						message = message.as_str(),
						"Ignored message because of rate limiting"
					);
					continue;
				}
			}

			debug!(
				?target,
				?invoker,
				message = message.as_str(),
				"Got message"
			);

			let msg = Message {
				target: *target,
				invoker: invoker.as_ref(),
				message,
			};
			if let Some(response) = bot.actions.handle(bot, con, &msg) {
				bot.rate_limiting.push(Instant::now());
				let state = con.get_state().unwrap();
				if let Err(error) =
					state.send_message(*target, response.as_ref()).send(con)
				{
					error!(%error, "Failed to send response")
				}
			}
		}
	}
}

fn escape_bb(s: &str) -> String { s.replace('[', "\\[") }
