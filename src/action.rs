use std::borrow::Cow;
use std::process::Command;

use failure::bail;
use regex::Regex;
use serde::{Deserialize, Serialize};
use slog::error;
use tsclientlib::{ConnectionLock, TextMessageTargetMode};

use crate::{Message, Result};

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActionDefinition {
	// Matcher
	/// Plain string
	pub contains: Option<String>,
	/// Regex
	pub regex: Option<String>,
	/// Check the chat mode for the message: Either `server`, `channel`,
	/// `client` or `poke`.
	pub chat: Option<String>,

	// Reaction
	/// A simple string response.
	pub response: Option<String>,
	/// Execute program
	pub command: Option<String>,
	/// Execute command in a shell
	pub shell: Option<String>,
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
	Function(Box<for<'a> Fn(&ConnectionLock, &'a Message) -> Option<Cow<'a, str>> + Send + Sync>),
}

impl ActionDefinition {
	pub fn to_action(&self) -> Result<Action> {
		// Condition
		let mut res = Action::default();
		if let Some(contains) = &self.contains {
			if let Some(matches) = &self.regex {
				bail!("An action can only have either contains or matches. \
					This one contains both ({} and {})", contains, matches);
			}
			// Only match string at word boundaries
			res.matchers.push(Matcher::Regex(Regex::new(&format!(r"\b{}\b",
				regex::escape(contains)))?));
		} else if let Some(matches) = &self.regex {
			res.matchers.push(Matcher::Regex(Regex::new(matches)?));
		}

		if let Some(chat) = &self.chat {
			let mode = match chat.as_str() {
				"server" => Some(TextMessageTargetMode::Server),
				"channel" => Some(TextMessageTargetMode::Channel),
				"client" => Some(TextMessageTargetMode::Client),
				"poke" => None,
				s => bail!("Chat mode must be server, channel, client or \
					poke. '{}' is not allowed.", s),
			};
			res.matchers.push(Matcher::Mode(mode));
		}

		// Reaction
		let mut counter = 0;
		if let Some(r) = &self.response {
			res.reaction = Some(Reaction::Plain(r.to_string()));
			counter += 1;
		}
		if let Some(c) = &self.command {
			res.reaction = Some(Reaction::Command(c.to_string()));
			counter += 1;
		}
		if let Some(s) = &self.shell {
			res.reaction = Some(Reaction::Shell(s.to_string()));
			counter += 1;
		}

		if counter > 1 {
			bail!("Only one reaction (response, command or shell) is allowed.");
		}

		Ok(res)
	}
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
	fn get_mode(m: &Option<TextMessageTargetMode>) -> &'static str {
		match m {
			Some(TextMessageTargetMode::Server) => "server",
			Some(TextMessageTargetMode::Channel) => "channel",
			Some(TextMessageTargetMode::Client) => "client",
			Some(TextMessageTargetMode::Unknown) => panic!("Unknown TextMessageTargetMode"),
			None => "poke",
		}
	}

	/// If `None` is returned, the next action should be tested.
	pub fn execute<'a>(&'a self, con: &ConnectionLock, msg: &'a Message) -> Option<Cow<'a, str>> {
		match self {
			Reaction::Plain(s) => Some(Cow::Borrowed(s.as_str())),
			Reaction::Command(s) |
			Reaction::Shell(s) => {
				let output;
				if let Reaction::Command(_) = self {
					// Split arguments at spaces
					let mut split = s.split(' ');
					output = Command::new(split.next().unwrap()).args(split)
						.output();
				} else {
					// Shell
					#[cfg(target_family = "unix")]
					{
						let mut cmd = Command::new("sh");
						cmd
							.arg("-c")
							.arg(s)
							// Program name
							.arg("sh")
							// Arguments
							.arg(Self::get_mode(&msg.mode))
							.arg(msg.invoker.name);
						if let Some(uid) = &msg.invoker.uid {
							cmd.arg(uid.0);
						}
						output = cmd.output();
					}

					#[cfg(not(target_family = "unix"))]
					{
						// TODO Look up how to use this on windows
						let mut cmd = Command::new("cmd");
						cmd
							.arg("/C")
							.arg(s)
							// Arguments
							.arg(Self::get_mode(&msg.mode))
							.arg(msg.invoker.name);
						if let Some(uid) = &msg.invoker.uid {
							cmd.arg(uid.0);
						}
						output = cmd.output();
					}
				}

				let output = match output {
					Ok(o) => o,
					Err(e) => {
						error!(crate::LOGGER, "Failed to execute shell";
							"command" => s, "error" => ?e);
						// Don't proceed
						return Some("".into());
					}
				};
				if !output.status.success() {
					// Skip and try next action
					return None;
				}

				// Try to parse result
				let res = match std::str::from_utf8(&output.stdout) {
					Ok(r) => r,
					Err(e) => {
						error!(crate::LOGGER, "Failed to parse output";
							"command" => s, "error" => ?e,
							"output" => ?output.stdout);
						// Don't proceed
						return Some("".into());
					}
				};

				Some(res.to_string().into())
			}
			Reaction::Function(f) => f(con, msg),
		}
	}
}

impl ActionList {
	pub fn handle<'a>(&'a self, con: &ConnectionLock, msg: &'a Message) -> Option<Cow<'a, str>> {
		'actions: for a in &self.0 {
			for m in &a.matchers {
				if !m.matches(msg) {
					continue 'actions;
				}
			}

			if let Some(a) = &a.reaction {
				if let Some(res) = a.execute(con, msg) {
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
