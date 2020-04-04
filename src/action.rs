use std::borrow::Cow;
use std::fmt;
use std::process::Command;

use anyhow::{bail, Result};
use regex::Regex;
use serde::{Deserialize, Serialize};
use slog::error;
use tsclientlib::{MessageTarget, TextMessageTargetMode};
use tsclientlib::facades::ConnectionMut;

use crate::{Bot, Message};

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

#[derive(Default, Debug)]
pub struct ActionList(pub Vec<Action>);

#[derive(Default, Debug)]
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

type ReactionFunction = Box<
	dyn for<'a> Fn(&Bot, &mut ConnectionMut, &'a Message) -> Option<Cow<'a, str>>
		+ Send
		+ Sync,
>;
pub enum Reaction {
	Plain(String),
	Command(String),
	Shell(String),
	Function(ReactionFunction),
}

impl fmt::Debug for Reaction {
	fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
		match self {
			Reaction::Plain(s) => write!(f, "Reaction::Plain({})", s),
			Reaction::Command(s) => write!(f, "Reaction::Command({})", s),
			Reaction::Shell(s) => write!(f, "Reaction::Shell({})", s),
			Reaction::Function(_) => write!(f, "Reaction::Function()"),
		}
	}
}

impl ActionDefinition {
	pub fn to_action(&self) -> Result<Action> {
		// Condition
		let mut res = Action::default();
		if let Some(contains) = &self.contains {
			if let Some(matches) = &self.regex {
				bail!(
					"An action can only have either contains or matches. This \
					 one contains both ({} and {})",
					contains,
					matches
				);
			}
			// Only match string at word boundaries
			// Add \b only if the first/last character is alphabetix.
			let mut regex = regex::escape(contains);
			if regex
				.chars()
				.next()
				.map(|c| c.is_alphabetic())
				.unwrap_or(false)
			{
				regex = format!(r"\b{}", regex);
			}
			if regex
				.chars()
				.last()
				.map(|c| c.is_alphabetic())
				.unwrap_or(false)
			{
				regex = format!(r"{}\b", regex);
			}
			res.matchers.push(Matcher::Regex(Regex::new(&regex)?));
		} else if let Some(matches) = &self.regex {
			res.matchers.push(Matcher::Regex(Regex::new(matches)?));
		}

		if let Some(chat) = &self.chat {
			let mode = match chat.as_str() {
				"server" => Some(TextMessageTargetMode::Server),
				"channel" => Some(TextMessageTargetMode::Channel),
				"client" => Some(TextMessageTargetMode::Client),
				"poke" => None,
				s => bail!(
					"Chat mode must be server, channel, client or poke. '{}' \
					 is not allowed.",
					s
				),
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
			Matcher::Mode(m) => match m {
				Some(TextMessageTargetMode::Server) => {
					if let MessageTarget::Server = msg.target {
						true
					} else {
						false
					}
				}
				Some(TextMessageTargetMode::Channel) => {
					if let MessageTarget::Channel = msg.target {
						true
					} else {
						false
					}
				}
				Some(TextMessageTargetMode::Client) => {
					if let MessageTarget::Client(_) = msg.target {
						true
					} else {
						false
					}
				}
				Some(TextMessageTargetMode::Unknown) => false,
				None => {
					if let MessageTarget::Poke(_) = msg.target {
						true
					} else {
						false
					}
				}
			},
		}
	}
}

impl Reaction {
	pub fn get_target(m: &MessageTarget) -> &'static str {
		match m {
			MessageTarget::Server => "server",
			MessageTarget::Channel => "channel",
			MessageTarget::Client(_) => "client",
			MessageTarget::Poke(_) => "poke",
		}
	}

	pub fn get_mode(m: &Option<TextMessageTargetMode>) -> &'static str {
		match m {
			Some(TextMessageTargetMode::Server) => "server",
			Some(TextMessageTargetMode::Channel) => "channel",
			Some(TextMessageTargetMode::Client) => "client",
			Some(TextMessageTargetMode::Unknown) => {
				panic!("Unknown TextMessageTargetMode")
			}
			None => "poke",
		}
	}

	/// If `None` is returned, the next action should be tested.
	pub fn execute<'a>(
		&'a self,
		bot: &Bot,
		con: &mut ConnectionMut,
		msg: &'a Message,
	) -> Option<Cow<'a, str>>
	{
		match self {
			Reaction::Plain(s) => Some(Cow::Borrowed(s.as_str())),
			Reaction::Command(s) | Reaction::Shell(s) => {
				let output;
				if let Reaction::Command(_) = self {
					// Split arguments at spaces
					let mut split = s.split(' ');
					let mut cmd = Command::new(split.next().unwrap());
					cmd.args(split)
						// Arguments
						.arg(Self::get_target(&msg.target))
						.arg(&msg.message)
						.arg(msg.invoker.name);
					if let Some(uid) = &msg.invoker.uid {
						cmd.arg(&base64::encode(&uid.0));
					}
					output = cmd.output();
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
							.arg(Self::get_target(&msg.target))
							.arg(&msg.message)
							.arg(msg.invoker.name);
						if let Some(uid) = &msg.invoker.uid {
							cmd.arg(&base64::encode(&uid.0));
						}
						output = cmd.output();
					}

					#[cfg(not(target_family = "unix"))]
					{
						// Windows is untested
						let mut cmd = Command::new("cmd");
						cmd
							.arg("/C")
							.arg(s)
							// Arguments
							.arg(Self::get_target(&msg.target))
							.arg(&msg.message)
							.arg(msg.invoker.name);
						if let Some(uid) = &msg.invoker.uid {
							cmd.arg(&base64::encode(&uid.0));
						}
						output = cmd.output();
					}
				}

				let output = match output {
					Ok(o) => o,
					Err(e) => {
						error!(bot.logger, "Failed to execute shell";
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
						error!(bot.logger, "Failed to parse output";
							"command" => s, "error" => ?e,
							"output" => ?output.stdout);
						// Don't proceed
						return Some("".into());
					}
				};

				Some(res.to_string().into())
			}
			Reaction::Function(f) => f(bot, con, msg),
		}
	}
}

impl ActionList {
	pub fn handle<'a>(
		&'a self,
		bot: &Bot,
		con: &mut ConnectionMut,
		msg: &'a Message,
	) -> Option<Cow<'a, str>>
	{
		'actions: for a in &self.0 {
			for m in &a.matchers {
				if !m.matches(msg) {
					continue 'actions;
				}
			}

			if let Some(a) = &a.reaction {
				if let Some(res) = a.execute(bot, con, msg) {
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
