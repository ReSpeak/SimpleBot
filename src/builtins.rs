use std::borrow::Cow;
use std::fs;
use std::path::Path;
use std::sync::Weak;

use futures::{future, Future};
use parking_lot::{Mutex, RwLock};
use regex::Regex;
use slog::{debug, error, info};
use tsclientlib::ConnectionLock;

use crate::{ActionFile, Bot, Message};
use crate::action::*;

/// Add builtin functions to the end of the action list.
pub fn init(b2: Weak<RwLock<Bot>>, bot: &mut Bot) {
	let p = regex::escape(&bot.settings.prefix);

	let help_regex = Regex::new(&format!("^{}help", p)).unwrap();
	add_fun(bot, help_regex, move |b, _, _| help(b));

	let list_mutex = Mutex::new(None);
	let list_regex = Regex::new(&format!("^{}list", p)).unwrap();
	add_fun(bot, list_regex, move |b, _, m| list(b, &list_mutex, m));

	let add_regex = Regex::new(&format!("^{}add",
		p)).unwrap();
	let long_add_regex = Regex::new(&format!("^{}add (?P<response>.*) on (?P<trigger>.*)$",
		p)).unwrap();
	let b = b2.clone();
	add_fun(bot, add_regex, move |bot, _, m| add(&b, bot, &long_add_regex, m));

	let del_regex = Regex::new(&format!("^{}del",
		p)).unwrap();
	let long_del_regex = Regex::new(&format!("^{}del (?P<trigger>.*)$",
		p)).unwrap();
	let b = b2.clone();
	add_fun(bot, del_regex, move |bot, _, m| del(&b, bot, &long_del_regex, m));

	let reload_regex = Regex::new(&format!("^{}reload$", p))
		.unwrap();
	add_fun(bot, reload_regex, move |_, _, _| {
		reload(&b2);
		Some("".into())
	});

	let quit_regex = Regex::new(&format!("^{}quit$", p))
		.unwrap();
	add_fun(bot, quit_regex, move |b, c, m| quit(b, c, m));
}

fn add_fun<F: for<'a> Fn(&Bot, &ConnectionLock, &'a Message) -> Option<Cow<'a, str>>
	+ Send + Sync + 'static>(bot: &mut Bot, r: Regex, f: F) {
	bot.actions.0.push(Action {
		matchers: vec![Matcher::Regex(r)],
		reaction: Some(Reaction::Function(Box::new(f))),
	});
}

fn add<'a>(bot: &Weak<RwLock<Bot>>, b: &Bot, r: &Regex, msg: &'a Message) -> Option<Cow<'a, str>> {
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
		regex: None,
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
	Some("".into())
}

/// Remove everything which matches this trigger.
fn del<'a>(bot: &Weak<RwLock<Bot>>, b: &Bot, r: &Regex, msg: &'a Message) -> Option<Cow<'a, str>> {
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

	let mut count = 0;
	dynamic.on_message.retain(|a| {
		let r = a.contains.as_ref().map(|c| c == trigger).unwrap_or(false);
		if !r {
			count += 1;
		}
		r
	});

	// Save
	if let Err(e) = fs::write(&path, &toml::to_vec(&dynamic).unwrap()) {
		error!(b.logger, "Failed to save dynamic actions"; "error" => ?e);
		return Some("Failed".into());
	}

	reload(bot);
	if count == 1 {
		Some(format!("Removed {} element", count).into())
	} else {
		Some(format!("Removed {} elements", count).into())
	}
}

fn reload(b: &Weak<RwLock<Bot>>) {
	if let Some(b2) = b.upgrade() {
		let b = b.clone();
		tokio::spawn(future::lazy(move ||{
			let mut bot = b2.write();

			match crate::load_settings(b, &mut bot) {
				Ok(()) => info!(bot.logger, "Reloaded successfully"),
				Err(e) => error!(bot.logger, "Failed to reload"; "error" => ?e),
			}
			Ok(())
		}));
	}
}

fn quit<'a>(bot: &Bot, con: &ConnectionLock, msg: &'a Message) -> Option<Cow<'a, str>> {
	info!(bot.logger, "Leaving on request"; "message" => ?msg);
	// We get no disconnect message here
	tokio::spawn(con.to_mut().remove()
		// Ignore errors on disconnect
		.map_err(move |_| ()));
	Some("".into())
}

/// Please do not remove the help message.
fn help<'a>(bot: &Bot) -> Option<Cow<'a, str>> {
	Some(format!("This is a [URL=https://github.com/ReSpeak/SimpleBot]SimpleBot[/URL].\n\
		Use [i]{prefix}add <reaction> on <trigger>[/i] to add new actions\n\
		or [i]{prefix}del <trigger>[/i] to remove them.\n\
		[i]{prefix}list[/i] lists all commands and actions.\n\
		[i]{prefix}quit[/i] disconnects the bot.", prefix = bot.settings.prefix)
		.into())
}

type ListPages = Vec<String>;
fn list<'a>(bot: &Bot, list: &Mutex<Option<ListPages>>, msg: &Message)
	-> Option<Cow<'a, str>> {
	let mut list = list.lock();
	if list.is_none() {
		let mut matchers = Vec::new();
		for a in &bot.actions.0 {
			let mut res = String::new();
			for m in &a.matchers {
				match m {
					Matcher::Regex(r) => {
						let mut r = r.as_str().to_string();
						r = r.replace(&['^', '$'][..], "");
						r = r.replace("\\b", "");

						r = r.replace("\\.", ".");
						res.push_str(&r);
					}
					Matcher::Mode(m) => res.push_str(&format!(" (only in {} mode)",
						Reaction::get_mode(m))),
				}
			}
			matchers.push(res);
		}
		matchers.sort_unstable();
		matchers.dedup();

		// Group lines so thet at maximum 900 chars are on one page
		// (there will be additional text later).
		let mut res = vec![String::new()];
		for m in matchers {
			if res.last().unwrap().len() + m.len() > 900 {
				res.push(String::new());
			}
			let cur = res.last_mut().unwrap();
			cur.push('\n');
			cur.push_str(&m);
		}

		*list = Some(res);
	}

	let list = list.as_ref().unwrap();

	let mut page = 0;
	if let Some(i) = msg.message.rfind(' ') {
		if let Ok(n) = (msg.message[i+1..]).parse::<usize>() {
			if n != 0 {
				// Start indexing at 1
				page = n - 1;
			}
		}
	}

	if page >= list.len() {
		page = list.len() - 1;
	}

	let page_s = list[page].clone();
	let res = if list.len() > 1 {
		format!("Page {}/{}, use [i]{}list <page>[/i] to show more.{}",
			page + 1,
			list.len(),
			bot.settings.prefix,
			page_s,
		)
	} else {
		page_s
	};

	Some(res.into())
}
