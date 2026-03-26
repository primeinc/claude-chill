mod cli;

use clap::Parser;
use claude_chill::config::Config;
use claude_chill::key_parser;
use claude_chill::proxy::{Proxy, ProxyConfig};
use log::debug;

fn main() {
    // File-based debug logging — only available in debug builds to avoid
    // exposing an arbitrary file-write capability in release binaries.
    #[cfg(debug_assertions)]
    if let Ok(log_file) = std::env::var("CLAUDE_CHILL_LOG_FILE") {
        use std::fs::OpenOptions;
        if let Ok(file) = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&log_file)
        {
            env_logger::Builder::new()
                .filter_level(log::LevelFilter::Debug)
                .target(env_logger::Target::Pipe(Box::new(file)))
                .init();
        }
    }

    let cli = cli::Cli::parse();
    let config = Config::load();

    let history_lines = cli.history_lines.unwrap_or(config.history_lines);

    let lookback_key = cli
        .lookback_key
        .clone()
        .unwrap_or_else(|| config.lookback_key.clone());

    let (lookback_sequence_legacy, lookback_sequence_kitty) = match key_parser::parse(&lookback_key)
    {
        Ok(key) => {
            let legacy = key.to_escape_sequence();
            let kitty = key.to_kitty_sequence().unwrap_or_else(|| legacy.clone());
            (legacy, kitty)
        }
        Err(e) => {
            eprintln!("Invalid lookback key '{lookback_key}': {e}");
            eprintln!("Using default: [ctrl][6]");
            (vec![0x1E], b"\x1b[54;5u".to_vec())
        }
    };

    debug!(
        "Lookback sequences: legacy={lookback_sequence_legacy:?} kitty={lookback_sequence_kitty:?}"
    );

    let auto_lookback_timeout_ms = cli
        .auto_lookback_timeout
        .unwrap_or(config.auto_lookback_timeout_ms);

    let proxy_config = ProxyConfig {
        max_history_lines: history_lines,
        lookback_key,
        lookback_sequence_legacy,
        lookback_sequence_kitty,
        auto_lookback_timeout_ms,
    };

    let cmd_args: Vec<&str> = cli.args.iter().map(|s| s.as_str()).collect();

    // The exit code is collected after Proxy is dropped (restoring terminal state).
    // We use std::process::exit() instead of ExitCode to preserve the full i32 range
    // (ExitCode::from(u8) truncates codes > 255).
    let code = match Proxy::spawn(&cli.command, &cmd_args, proxy_config) {
        Ok(mut proxy) => match proxy.run() {
            Ok(exit_code) => exit_code,
            Err(e) => {
                eprintln!("Proxy error: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("Failed to start proxy: {e:#}");
            1
        }
    };
    // Proxy is dropped here, restoring terminal state before exit.
    std::process::exit(code)
}
