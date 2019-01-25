use std::borrow::Cow;
use std::fs;
use std::path::Path;
use std::sync::{Arc, Weak};

use futures::{future, Future};
use parking_lot::RwLock;
use regex::Regex;
use slog::{debug, error, info, Logger};
use tsclientlib::ConnectionLock;

use crate::{ActionFile, Bot, Message};
use crate::action::*;

/// Add builtin functions to the end of the action list.
pub fn init(b2: Weak<RwLock<Bot>>, bot: &mut Bot) {
	let add_regex = Regex::new(&format!("^{}add",
		bot.settings.prefix)).unwrap();
	let long_add_regex = Regex::new(&format!("^{}add (?P<response>.*) on (?P<trigger>.*)$",
		bot.settings.prefix)).unwrap();
	let b = b2.clone();
	add_fun(bot, add_regex, move |_, m| add(&b, &long_add_regex, m));

	let del_regex = Regex::new(&format!("^{}del",
		bot.settings.prefix)).unwrap();
	let long_del_regex = Regex::new(&format!("^{}del (?P<trigger>.*)$",
		bot.settings.prefix)).unwrap();
	let b = b2.clone();
	add_fun(bot, del_regex, move |_, m| del(&b, &long_del_regex, m));

	let reload_regex = Regex::new(&format!("^{}reload$", bot.settings.prefix))
		.unwrap();
	add_fun(bot, reload_regex, move |_, _| {
		reload(&b2);
		None
	});

	let logger = bot.logger.clone();
	let quit_regex = Regex::new(&format!("^{}quit$", bot.settings.prefix))
		.unwrap();
	add_fun(bot, quit_regex, move |c, m| quit(&logger, c, m));
}

fn add_fun<F: for<'a> Fn(&ConnectionLock, &'a Message) -> Option<Cow<'a, str>>
	+ Send + Sync + 'static>(bot: &mut Bot, r: Regex, f: F) {
	bot.actions.0.push(Action {
		matchers: vec![Matcher::Regex(r)],
		reaction: Some(Reaction::Function(Box::new(f))),
	});
}

fn add<'a>(bot: &Weak<RwLock<Bot>>, r: &Regex, msg: &'a Message) -> Option<Cow<'a, str>> {
	let b2 = match bot.upgrade() {
		Some(r) => r,
		None => return None,
	};
	let b = b2.read();
	let caps = match r.captures(msg.message) {
		Some(r) => r,
		None => return Some(format!("Usage: {}add <response> on <trigger>",
			b.settings.prefix).into()),
	};
	let response = caps.name("response").unwrap();
	let trigger = caps.name("trigger").unwrap();

	// Load
	let path = Path::new(&b.settings.dynamic_actions);
	let path = if path.is_absolute() {
		path.into()
	} else {
		b.base_dir.join(path)
	};
	let mut dynamic: ActionFile = match fs::read_to_string(&path) {
		Ok(s) => match toml::from_str(&s) {
			Ok(r) => r,
			Err(e) => {
				error!(b.logger, "Failed to parse dynamic actions";
					"error" => ?e);
				return Some("Failed".into());
			}
		}
		Err(e) => {
			debug!(b.logger, "Dynamic actions not loaded"; "error" => %e);
			ActionFile::default()
		}
	};

	dynamic.on_message.push(ActionDefinition {
		contains: Some(trigger.as_str().into()),
		matches: None,
		chat: None,

		response: Some(response.as_str().into()),
		command: None,
		shell: None,
	});

	// Save
	if let Err(e) = fs::write(&path, &toml::to_vec(&dynamic).unwrap()) {
		error!(b.logger, "Failed to save dynamic actions"; "error" => ?e);
		return Some("Failed".into());
	}

	reload(bot);
	None
}

/// Remove everything which matches this trigger.
fn del<'a>(bot: &Weak<RwLock<Bot>>, r: &Regex, msg: &'a Message) -> Option<Cow<'a, str>> {
	let b2 = match bot.upgrade() {
		Some(r) => r,
		None => return None,
	};
	let b = b2.read();
	let caps = match r.captures(msg.message) {
		Some(r) => r,
		None => return Some(format!("Usage: {}del <trigger>", b.settings.prefix)
			.into()),
	};
	let trigger = caps.name("trigger").unwrap().as_str();

	// Load
	let path = Path::new(&b.settings.dynamic_actions);
	let path = if path.is_absolute() {
		path.into()
	} else {
		b.base_dir.join(path)
	};
	let mut dynamic: ActionFile = match fs::read_to_string(&path) {
		Ok(s) => match toml::from_str(&s) {
			Ok(r) => r,
			Err(e) => {
				error!(b.logger, "Failed to parse dynamic actions";
					"error" => ?e);
				return Some("Failed".into());
			}
		}
		Err(e) => {
			debug!(b.logger, "Dynamic actions not loaded"; "error" => %e);
			ActionFile::default()
		}
	};

	dynamic.on_message.retain(|a| {
		a.contains.as_ref().map(|c| c == trigger).unwrap_or(false)
	});

	// Save
	if let Err(e) = fs::write(&path, &toml::to_vec(&dynamic).unwrap()) {
		error!(b.logger, "Failed to save dynamic actions"; "error" => ?e);
		return Some("Failed".into());
	}

	reload(bot);
	None
}

fn reload(bot: &Weak<RwLock<Bot>>) {
	if let Some(b2) = bot.upgrade() {
		tokio::spawn(future::lazy(move ||{
			let mut bot = b2.write();

			match crate::load_settings(
				&bot.logger,
				&bot.base_dir,
				&bot.settings_path,
				&bot.settings,
			) {
				Ok((settings, actions)) => {
					bot.settings = settings;
					bot.actions = actions;
					crate::builtins::init(Arc::downgrade(&b2), &mut *bot);
					info!(bot.logger, "Reloaded successfully");
				}
				Err(e) => {
					error!(bot.logger, "Failed to reload"; "error" => ?e);
				}
			}
			Ok(())
		}));
	}
}

fn quit<'a>(logger: &Logger, con: &ConnectionLock, msg: &'a Message) -> Option<Cow<'a, str>> {
	info!(logger, "Leaving on request"; "message" => ?msg);
	// We get no disconnect message here
	tokio::spawn(con.to_mut().remove()
		// Ignore errors on disconnect
		.map_err(move |_| ()));
	None
}
