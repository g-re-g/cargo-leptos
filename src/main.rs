mod config;
mod logger;
mod run;
pub mod util;

use anyhow::Result;
use binary_install::Cache;
use clap::{Parser, Subcommand, ValueEnum};
use config::Config;
use run::{
    assets, cargo,
    new::NewCommand,
    reload, sass, serve, wasm,
    watch::{self, Watched},
    Html,
};
use std::{env, path::PathBuf};
use tokio::{
    signal,
    sync::{broadcast, RwLock},
};
use util::{wait_for, PathBufAdditions};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    /// sent by ctrl-c
    ShutDown,
    /// sent when a source file is changed
    SrcChanged,
    /// sent when an asset file changed
    AssetsChanged(Watched),
    /// sent when a style file changed
    StyleChanged,
    /// messages sent to reload server (forwarded to browser)
    Reload(String),
}

lazy_static::lazy_static! {
    /// Interrupts current serve or cargo operation. Used for watch
    pub static ref MSG_BUS: broadcast::Sender<Msg> = {
        broadcast::channel(10).0
    };
    pub static ref SHUTDOWN: RwLock<bool> = RwLock::new(false);
    pub static ref INSTALL_CACHE: Cache = Cache::new("cargo-leptos").unwrap();
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, ValueEnum)]
pub enum Log {
    /// WASM build (wasm, wasm-opt, walrus)
    Wasm,
    /// Internal reload and csr server (hyper, axum)
    Server,
}

#[derive(Debug, Clone, Parser, PartialEq, Default)]
pub struct Opts {
    /// Build artifacts in release mode, with optimizations.
    #[arg(short, long)]
    release: bool,

    /// Build for client side rendering instead of the default hydrate mode. Useful during development due to faster compile times.
    #[arg(long)]
    csr: bool,

    /// Verbosity (none: info, errors & warnings, -v: verbose, --vv: very verbose).
    #[arg(short, action = clap::ArgAction::Count)]
    verbose: u8,
}

#[derive(Debug, Parser)]
#[clap(version)]
pub struct Cli {
    /// Path to Cargo.toml.
    #[arg(long)]
    manifest_path: Option<String>,

    /// Output logs from dependencies (multiple --log accepted).
    #[arg(long)]
    log: Vec<Log>,

    #[command(subcommand)]
    command: Commands,
}

#[derive(Debug, Subcommand, PartialEq)]
enum Commands {
    /// Output toml that needs to be added to the Cargo.toml file.
    Config,
    /// Compile the project. Defaults to hydrate mode.
    Build(Opts),
    /// Run the cargo tests for app, client and server.
    Test(Opts),
    /// Serve. Defaults to hydrate mode.
    Serve(Opts),
    /// Serve and automatically reload when files change. Defaults to hydrate mode.
    Watch(Opts),
    /// Start wizard for creating a new project (using cargo-generate)
    New(NewCommand),
}

#[tokio::main]
async fn main() -> Result<()> {
    let mut args: Vec<String> = env::args().collect();
    // when running as cargo leptos, the second argument is "leptos" which
    // clap doesn't expect
    if args.get(1).map(|a| a == "leptos").unwrap_or(false) {
        args.remove(1);
    }

    let args = Cli::parse_from(&args);

    if let Commands::New(new) = &args.command {
        return new.run().await;
    }

    if let Some(path) = &args.manifest_path {
        let path = PathBuf::from(path).without_last();
        std::env::set_current_dir(path)?;
    }

    let opts = match &args.command {
        Commands::New(_) => panic!(""),
        Commands::Config => return Ok(println!(include_str!("leptos.toml"))),
        Commands::Build(opts)
        | Commands::Serve(opts)
        | Commands::Test(opts)
        | Commands::Watch(opts) => opts,
    };
    logger::setup(opts.verbose, &args.log);

    let config = config::read(&args, opts.clone())?;

    tokio::spawn(async {
        signal::ctrl_c().await.expect("failed to listen for event");
        log::info!("Leptos ctrl-c received");
        *SHUTDOWN.write().await = true;
        MSG_BUS.send(Msg::ShutDown).unwrap();
    });

    match args.command {
        Commands::Config | Commands::New(_) => panic!(),
        Commands::Build(_) => build(&config, true).await,
        Commands::Serve(_) => serve(&config).await,
        Commands::Test(_) => cargo::test(&config).await,
        Commands::Watch(_) => watch(&config).await,
    }
}

async fn send_reload() {
    if !*SHUTDOWN.read().await {
        if let Err(e) = MSG_BUS.send(Msg::Reload("reload".to_string())) {
            log::error!("Leptos failed to send reload: {e}");
        }
    }
}
async fn build(config: &Config, copy_assets: bool) -> Result<()> {
    log::debug!(r#"Leptos cleaning contents of "target/site/pkg""#);
    util::rm_dir_content("target/site/pkg")?;
    if copy_assets {
        assets::update(config)?;
    }
    build_client(&config).await?;

    if !config.cli.csr {
        cargo::build(&config, false).await?;
    }
    Ok(())
}
async fn build_client(config: &Config) -> Result<()> {
    sass::run(&config).await?;

    let html = Html::read(&config.leptos.index_file)?;

    if config.cli.csr {
        wasm::build(&config).await?;
        html.generate_html(&config)?;
    } else {
        wasm::build(&config).await?;
        html.generate_rust(&config)?;
    }
    Ok(())
}

async fn serve(config: &Config) -> Result<()> {
    build(&config, true).await?;
    if config.cli.csr {
        serve::spawn(&config).await.await?;
        Ok(())
    } else {
        cargo::run(&config).await
    }
}

async fn watch(config: &Config) -> Result<()> {
    let _ = watch::spawn(config).await?;

    if let Some(assets_dir) = &config.leptos.assets_dir {
        let _ = assets::spawn(assets_dir).await?;
    }
    if config.cli.csr {
        serve::spawn(&config).await;
    }

    reload::spawn(&config).await;

    loop {
        match build(config, false).await {
            Ok(_) => {
                send_reload().await;
                if config.cli.csr {
                    wait_for(&[Msg::SrcChanged, Msg::ShutDown]).await;
                } else {
                    cargo::run(&config).await?;
                }
                if *SHUTDOWN.read().await {
                    break;
                } else {
                    log::info!("Leptos ===================== rebuilding =====================");
                }
            }
            Err(e) => {
                log::warn!("Leptos rebuild stopped due to error: {e}");
                while MSG_BUS.subscribe().recv().await? != Msg::SrcChanged {}
            }
        }
    }
    Ok(())
}
