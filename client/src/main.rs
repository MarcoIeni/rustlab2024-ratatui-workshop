use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use clap::Parser;
use clap_verbosity_flag::{InfoLevel, Verbosity};
use color_eyre::eyre::{Context, OptionExt};
use colored_json::CompactFormatter;
use serde_json::Value;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
};
use tracing::level_filters::LevelFilter;
use tracing_log::AsTrace;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> color_eyre::Result<()> {
    color_eyre::install()?;
    let args = Args::parse();

    let level = args.verbosity.log_level_filter().as_trace();
    init_tracing(level);

    let mut stream = TcpStream::connect(args.address()).await?;
    tracing::info!("Connected to server: {}", stream.local_addr()?);

    let (reader, mut writer) = stream.split();
    let mut server_lines = BufReader::new(reader).lines();

    let stdin = tokio::io::stdin();
    let mut stdin_lines = BufReader::new(stdin).lines();

    let json_formatter = colored_json::ColoredFormatter::new(CompactFormatter);

    loop {
        tokio::select! {
            line = stdin_lines.next_line() => {
                let line = line.wrap_err("failed to read line from stdin")?;
                let line = line.ok_or_eyre("empty line from stdin")?;
                writer.write_all(line.as_bytes()).await?;
                writer.write_all(b"\n").await?;
            },
            line = server_lines.next_line() => {
                let line = line.wrap_err("failed to read line from server")?;
                let line = line.ok_or_eyre("empty line from server")?;
                // add some color to the JSON output
                let line: Value = serde_json::from_str(&line)?;
                let line = json_formatter.clone().to_colored_json_auto(&line)?;
                println!("{line}");
            },
            else => break,
        }
    }
    Ok(())
}

#[derive(Debug, clap::Parser)]
struct Args {
    /// The hostname of the server to connect to
    #[arg(short, long, default_value_t = Ipv4Addr::LOCALHOST.into())]
    ip: IpAddr,

    /// The port to connect to
    #[arg(short, long, default_value_t = 42069)]
    port: u16,

    /// Verbosity flags
    ///
    /// Automatically parses one or more --verbose and --quiet flags to set the log level.
    /// Default level is INFO. Use -v to increase the log level, and -q to decrease it.
    #[command(flatten)]
    verbosity: Verbosity<InfoLevel>,
}

impl Args {
    fn address(&self) -> SocketAddr {
        SocketAddr::new(self.ip, self.port)
    }
}

pub fn init_tracing(level_filter: LevelFilter) {
    let env_filter = EnvFilter::builder()
        .with_default_directive(level_filter.into())
        .from_env_lossy();
    tracing_subscriber::fmt()
        .with_env_filter(env_filter)
        .with_target(false)
        .without_time()
        .init();
}
