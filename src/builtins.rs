// TODO
#![allow(unused_variables)]
use std::borrow::Cow;
use std::sync::{Arc, Weak};

use futures::{future, Future};
use parking_lot::RwLock;
use regex::Regex;
use slog::{error, info, warn, Logger};
use tsclientlib::ConnectionLock;

use crate::{Bot, Message};
use crate::action::*;

/// Add builtin functions to the end of the action list.
pub fn init(b2: Weak<RwLock<Bot>>, bot: &mut Bot) {
	let add_regex = Regex::new(&format!("{}add (?P<response>.*) on (?P<trigger>.*)",
		bot.settings.prefix)).unwrap();
	add_fun(bot, add_regex.clone(), move |c, m| add(&add_regex, c, m));

	let del_regex = Regex::new(&format!("{}del (?P<trigger>.*)",
		bot.settings.prefix)).unwrap();
	add_fun(bot, del_regex.clone(), move |c, m| del(&del_regex, c, m));

	let reload_regex = Regex::new(&format!("{}reload", bot.settings.prefix))
		.unwrap();
	add_fun(bot, reload_regex, move |_, _| reload(&b2));

	let logger = bot.logger.clone();
	let quit_regex = Regex::new(&format!("{}quit", bot.settings.prefix))
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

fn add<'a>(r: &Regex, con: &ConnectionLock, msg: &'a Message) -> Option<Cow<'a, str>> {
	None
}

fn del<'a>(r: &Regex, con: &ConnectionLock, msg: &'a Message) -> Option<Cow<'a, str>> {
	None
}

fn reload<'a>(bot: &Weak<RwLock<Bot>>) -> Option<Cow<'a, str>> {
	if let Some(b2) = bot.upgrade() {
		tokio::spawn(future::lazy(move ||{
			let mut bot = b2.write();

			// TODO Only have this code once
			// Reload settings
			match std::fs::read_to_string(&bot.settings_path) {
				Ok(r) => {
					match toml::from_str(&r) {
						Ok(settings) => {
							bot.settings = settings;
						}
						Err(e) => {
							error!(bot.logger, "Failed to parse settings while \
								reloading"; "error" => ?e);
							return Ok(());
						}
					}
				}
				Err(e) => {
					// Only a soft error
					warn!(bot.logger, "Failed to read settings while \
						reloading, using defaults"; "error" => ?e);
				}
			}

			// Reload actions
			let mut actions = ActionList::default();
			if let Err(e) = crate::load_actions(&bot.base_dir, &mut actions,
				&bot.settings.actions) {
				error!(bot.logger, "Failed to load actions while reloading";
				   "error" => %e);
			}
			bot.actions = actions;
			crate::builtins::init(Arc::downgrade(&b2), &mut *bot);
			info!(bot.logger, "Reloaded successfully");
			// TODO Respond
			Ok(())
		}));
	}
	None
}

fn quit<'a>(logger: &Logger, con: &ConnectionLock, msg: &'a Message) -> Option<Cow<'a, str>> {
	info!(logger, "Leaving on request"; "message" => ?msg);
	// We get no disconnect message here
	let logger = logger.clone();
	tokio::spawn(con.to_mut().remove()
		// Ignore errors on disconnect
		.map_err(move |e| ()/*error!(logger,
			"Failed to disconnect";
			"error" => ?e)*/));
	None
}
