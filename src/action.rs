use std::borrow::Cow;

use regex::Regex;
use serde::Deserialize;
use tsclientlib::{InvokerRef, TextMessageTargetMode};

use crate::Message;

#[derive(Clone, Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDefinition {
	// Matcher
	/// Plain string
	contains: Option<String>,
	/// Regex
	matches: Option<String>,
	/// Check the chat mode for the message: Either `server`, `channel`,
	/// `client` or `poke`.
	chat: Option<String>,

	// Reaction
	/// A simple string response.
	response: Option<String>,
	/// Execute program
	command: Option<String>,
	/// Execute command in a shell
	shell: Option<String>,
}

#[derive(Default)]
pub struct ActionList(pub Vec<Action>);

#[derive(Default)]
pub struct Action {
	/// All matchers have to match for the reaction to be executed.
	pub matchers: Vec<Matcher>,
	/// If empty and this action matches, no action will be executed.
	pub reaction: Option<Reaction>,
}

#[derive(Clone, Debug)]
pub enum Matcher {
	Regex(Regex),
	/// If this is `None`, it means poke.
	Mode(Option<TextMessageTargetMode>),
}

pub enum Reaction {
	Plain(String),
	Command(String),
	Shell(String),
	Function(Box<for<'a> Fn(&'a Message) -> Option<Cow<'a, str>> + Send>),
}

impl Matcher {
	pub fn matches(&self, msg: &Message) -> bool {
		match self {
			Matcher::Regex(r) => r.is_match(msg.message),
			Matcher::Mode(m) => *m == msg.mode,
		}
	}
}

impl Reaction {
	/// If `None` is returned, the next action should be tested.
	pub fn execute<'a>(&'a self, msg: &'a Message) -> Option<Cow<'a, str>> {
		match self {
			Reaction::Plain(s) => Some(Cow::Borrowed(s.as_str())),
			Reaction::Command(s) => None, // TODO
			Reaction::Shell(s) => None, // TODO
			Reaction::Function(f) => f(msg),
		}
	}
}

impl ActionList {
	pub fn handle<'a>(&'a self, msg: &'a Message) -> Option<Cow<'a, str>> {
		'actions: for a in &self.0 {
			for m in &a.matchers {
				if !m.matches(msg) {
					continue 'actions;
				}
			}

			if let Some(a) = &a.reaction {
				if let Some(res) = a.execute(msg) {
					if res == "" {
						return None;
					} else {
						return Some(res);
					}
				}
			} else {
				return None;
			}
		}
		None
	}
}
