use std::borrow::Cow;

use lazy_static::lazy_static;
use regex::Regex;

use crate::{Bot, Message};
use crate::action::*;

/// Add builtin functions to the end of the action list.
pub fn init(bot: &mut Bot) {
	let add_regex = Regex::new(&format!("{}add (?P<response>.*) on (?P<trigger>.*)",
		bot.settings.prefix)).unwrap();
	add_fun(bot, add_regex.clone(), move |m| add(&add_regex, m));

	let del_regex = Regex::new(&format!("{}del (?P<trigger>.*)",
		bot.settings.prefix)).unwrap();
	add_fun(bot, del_regex.clone(), move |m| del(&del_regex, m));

	let reload_regex = Regex::new(&format!("{}reload", bot.settings.prefix))
		.unwrap();
	add_fun(bot, reload_regex, reload);

	let quit_regex = Regex::new(&format!("{}quit", bot.settings.prefix))
		.unwrap();
	add_fun(bot, quit_regex, quit);
}

fn add_fun<F: for<'a> Fn(&'a Message) -> Option<Cow<'a, str>> + Send + 'static>(bot: &mut Bot, r: Regex, f: F) {
	bot.actions.0.push(Action {
		matchers: vec![Matcher::Regex(r)],
		reaction: Some(Reaction::Function(Box::new(f))),
	});
}

fn add<'a>(r: &Regex, msg: &'a Message) -> Option<Cow<'a, str>> {
	None
}

fn del<'a>(r: &Regex, msg: &'a Message) -> Option<Cow<'a, str>> {
	None
}

fn reload<'a>(msg: &'a Message) -> Option<Cow<'a, str>> {
	None
}

fn quit<'a>(msg: &'a Message) -> Option<Cow<'a, str>> {
	None
}
