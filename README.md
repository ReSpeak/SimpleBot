# SimpleBot [![Build status](https://ci.appveyor.com/api/projects/status/p2og3vtd60boblbw/branch/master?svg=true)](https://ci.appveyor.com/project/Flakebi/simplebot/branch/master)

This is a simple TeamSpeak chat bot. It connects as a usual client and responds to some messages. The bot originally was built to test and showcase the capabilities of the [tsclientlib](https://github.com/ReSpeak/tsclientlib) library, though it developed into a more sophisticated and usable bot.

## Dependencies
- [OpenSSL](https://www.openssl.org) 1.1
- [Rust](https://rust-lang.org) (only needed if you want to compile the bot yourself, the preferred installation method is [rustup](https://rustup.rs)), currently the nightly version is needed

## Usage
Use a settings file with `./simple-bot --settings settings.toml` or put it in the configuration directory of your system, which should be

- Linux: `.config/simple-bot/`
- Windows: `%APPDATA%/ReSpeak/simple-bot/config/`
- macOS: `Library/Preferences/ReSpeak.simple-bot/`

## Features
The bot gets triggered by certain words, which then leads to a response.

These actions can be defined e.g. using the builtin commands.
These can be used in any chat with the bot (you can even poke him with your requests).
```
.help
# List all commands
.list <page>
.add <reaction> on <trigger>
.del <trigger>
# Reload the configuration
.reload
# Disconnect the bot
.quit
```

Examples:
```
.list
.add Please read the [URL=…]faq[/URL] on question
# Trigger the response
I have a question
# Will not trigger the response
This is questionable.
.del question
```

**Note**: The `trigger` will only match whole words. So in the example before, the response will be triggered on `I have a question` but not on `This is questionable`.
The matching is case sensitive, so `Question` will also not trigger the response.

## Configuration
The bot supports a simple configuration in a `settings.toml` file. The default options are
```toml
# The server to connect to
address = "localhost"
# The name of the bot
name = "SimpleBot"
# How many responses can be sent per second
rate_limit = 2
# The prefix for builtin commands
prefix = "."

# The path to the private key file
key_file = "private.key"
# The file to store dynamically added actions
dynamic_actions = "dynamic.toml"
```

Additionally, more complex behaviour can be defined in the configuration file.
This allows triggers on regular expressions instead of static strings and also gives the ability to execute arbitrary scripts.
The bot will first search for a matching action in the settings, then in the builtins and afterwards in the dynamic actions (the ones which were added with `.add`).
The format is:
```toml
[[actions.on_message]]
# Trigger
# contains and regex cannot be defined together, though it is ok to define none
# of them which will match every message.
contains = "simple string, like added with .add"
regex = "(?i)e.g. case invariant"
# The way the message is received.
chat = "server|channel|client|poke"

# Reaction
# At maximum one of the reactions can be defined
# A response of this type is added by the .add builtin command.
response = "plain response"
# Run a script, the arguments will be splitted at spaces and the following
# arguments will be added:
# - Chat mode (server|channel|client|poke)
# - Message
# - Username
# - User uid (optional): This can be used to uniquely identify a user, it will
#   not be set when a global server message is received.
command = "python3 ./script.py"
# Run the command in a shell so pipes can be used, etc. The same arguments as
# for commands will be passed, make sure to escape them!
shell = "echo Hi, \"$3\""
```

If a command is executed and returns `-1` as status code, the action of this command will be skipped and the next matching action will be executed.
This can be used to e.g. allow only certain users to quit the bot:
```toml
[[actions.on_message]]
regex = "^\\.(del|quit)"
# This is unix specific, on windows this should be another command.
shell = "grep -Fqv \"$4\" whitelist.txt"
```
And create a file `whitelist.txt`:
```
One uid per line
```

For every incomming message that starts with `.del` or `.quit`, the uid will be searched in the whitelist file.
If the uid is found, `grep` will exit with code `1`, the bot will skip this action and the `.quit` command will be executed.
If the uid is *not* found, `grep` will exit with code `0` and the bot will respond with the command output and not execute `.quit`. As the command output is empty, it will be ignored.

## License
Licensed under either of

 * [Apache License, Version 2.0](LICENSE-APACHE)
 * [MIT license](LICENSE-MIT)

at your option.
