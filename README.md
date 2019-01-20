This is a simple TeamSpeak bot. It connects as a usual client and responds to some messages. The bot is mostly built to test and showcase the capabilities of the [tsclientlib](https://github.com/ReSpeak/tsclientlib) library.

## Dependencies
- [OpenSSL](https://www.openssl.org) 1.1
- [Rust](https://rust-lang.org) (preferred installation method is [rustup](https://rustup.rs)), currently the nightly version is needed

## Usage
The bot supports a simple configuration in a `settings.toml` file. The default options are
```toml
name = "SimpleBot"
address = localhost
```

Use a settings file with `./simple-bot --settings settings.toml` or put it in the configuration directory of your system, which should be

- Linux: `.config/simple-bot/`
- Windows: `RoamingAppData/ReSpeak/simple-bot/config/`
- macOS: `Library/Preferences/ReSpeak.simple-bot/`

## Features
- The bot greets you if you say hi.
- If you ask how to quit the bot, it will give you a hint.

That’s all for now, it should stay simple and comprehensive.

## License
Licensed under either of

 * [Apache License, Version 2.0](LICENSE-APACHE)
 * [MIT license](LICENSE-MIT)

at your option.
