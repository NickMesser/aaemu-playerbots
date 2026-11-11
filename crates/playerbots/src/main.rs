//! `playerbots`: headless ArcheAge 1.2 player bots for the AAEmu Rust server.
//!
//! ```text
//! playerbots example-config > playerbots.toml
//! playerbots bake --pak <client>/game_pak --out <dir> --compact <compact.sqlite3>
//! playerbots run --config playerbots.toml [--count N] [--duration-secs S]
//! playerbots grant-gm-sql --config playerbots.toml [--game-db aaemu_game] [--login-db aaemu_login]
//! playerbots mcp [--url http://127.0.0.1:8790] [--token-env PLAYERBOTS_WEB_TOKEN]
//! ```

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use clap::{Parser, Subcommand};
use pb_web::BotHub;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use aa_world::{World, WorldPaths};
use playerbots::archetype::Archetypes;
use playerbots::bot::coordinator::{self, Dispatch};
use playerbots::bot::{Shared, LOGIN_PERMITS};
use playerbots::config::Config;
use playerbots::hub::{Hub, HubParams, SupCmd};
use playerbots::issues::Issues;
use playerbots::population::{serverapi, Population};
use playerbots::roster::{Roster, SharedRoster};
use playerbots::secrets::{self, Source};
use playerbots::supervisor::{ensure_roster, register_slots, Supervisor};

/// Headless ArcheAge 1.2 player bots for the AAEmu Rust server.
#[derive(Parser)]
#[command(name = "playerbots", version, about)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the bots (and the web UI).
    Run {
        /// The configuration file.
        #[arg(long, default_value = "playerbots.toml")]
        config: PathBuf,
        /// Number of bots to start (overrides `bots.count`).
        #[arg(long)]
        count: Option<u32>,
        /// Stop after this many seconds (default: run until Ctrl+C).
        #[arg(long)]
        duration_secs: Option<u64>,
    },
    /// Extract terrain, zones, housing areas, quest spheres and the world map from the client's
    /// `game_pak` (needed once; the bots work without it on flat terrain).
    Bake {
        /// The client's `game_pak`.
        #[arg(long)]
        pak: PathBuf,
        /// Output directory (`data.baked` in the config).
        #[arg(long)]
        out: PathBuf,
        /// World to bake.
        #[arg(long, default_value = "main_world")]
        world: String,
        /// Metres between height samples (a multiple of 2 that divides 1024).
        #[arg(long, default_value_t = 4.0)]
        step: f32,
        /// The server's `compact.sqlite3`: validates housing areas and gives the map rectangle.
        #[arg(long)]
        compact: Option<PathBuf>,
    },
    /// Print the SQL that gives the bot accounts GM access (for `bots.gm_bootstrap`).
    GrantGmSql {
        /// The configuration file.
        #[arg(long, default_value = "playerbots.toml")]
        config: PathBuf,
        /// The game server's MySQL database (accounts, characters).
        #[arg(long, default_value = playerbots::gm::DEFAULT_GAME_DB)]
        game_db: String,
        /// The login server's MySQL database (users).
        #[arg(long, default_value = playerbots::gm::DEFAULT_LOGIN_DB)]
        login_db: String,
    },
    /// Print the commented example configuration.
    ExampleConfig,
    /// Serve MCP (the Model Context Protocol) on stdin and stdout and forward the tool calls to
    /// the web API of a running `playerbots run`, so that an MCP client (Claude Code, Claude
    /// Desktop) can read and drive the bots. The log goes to stderr.
    Mcp {
        /// The address of the web UI.
        #[arg(long, default_value = "http://127.0.0.1:8790")]
        url: String,
        /// The NAME of the environment variable that holds the web token (`[web] token`); the
        /// token itself never goes on the command line.
        #[arg(long)]
        token_env: Option<String>,
    },
}

fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::new("info,playerbots=info"));
    if matches!(cli.command, Command::Mcp { .. }) {
        // (stdout carries the protocol: the log must not end up there)
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_writer(std::io::stderr)
            .with_ansi(false)
            .init();
    } else {
        tracing_subscriber::fmt().with_env_filter(filter).init();
    }
    match cli.command {
        Command::ExampleConfig => {
            print!("{}", Config::example());
            Ok(())
        }
        Command::GrantGmSql {
            config,
            game_db,
            login_db,
        } => {
            let cfg = Config::load(&config)?;
            // The accounts are the roster's, named one by one: a pattern like `pb[0-9]+` would
            // also grant GM to whoever registers `pb99999` on a server with AutoAccount.
            let roster = Roster::load(&cfg.bots.roster)?;
            let accounts: Vec<String> =
                roster.entries().iter().map(|e| e.account.clone()).collect();
            let sql = playerbots::gm::grant_sql(&accounts, &game_db, &login_db)
                .map_err(anyhow::Error::msg)?;
            let resolved =
                secrets::resolve_from_config(&cfg, &cfg.bots.roster, !roster.is_empty())?;
            if resolved.password.is_weak() {
                eprintln!(
                    "warning: the bot accounts use the public default password; anybody who knows it can log in \
                     as a GM once the SQL below is applied. Set server.password (or ${}) first.",
                    playerbots::config::PASSWORD_ENV
                );
            }
            print!("{sql}");
            Ok(())
        }
        Command::Bake {
            pak,
            out,
            world,
            step,
            compact,
        } => bake(pak, out, world, step, compact),
        Command::Mcp { url, token_env } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(1)
                .enable_all()
                .thread_name("playerbots-mcp")
                .build()
                .context("starting the async runtime")?;
            rt.block_on(playerbots::agent::mcp::run(&url, token_env.as_deref()))
        }
        Command::Run {
            config,
            count,
            duration_secs,
        } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .thread_name("playerbots")
                .build()
                .context("starting the async runtime")?;
            rt.block_on(run(config, count, duration_secs))
        }
    }
}

fn bake(
    pak: PathBuf,
    out: PathBuf,
    world: String,
    step: f32,
    compact: Option<PathBuf>,
) -> anyhow::Result<()> {
    let mut opts = aa_bake::BakeOptions::new(pak, out, world);
    opts.step = step;
    opts.compact = compact;
    let report = aa_bake::bake(&opts)?;
    println!("{report}");
    if report.checks.iter().any(|c| !c.ok) {
        bail!("some bake checks failed (see above)");
    }
    Ok(())
}

/// Says where the bot password comes from (never the password itself).
fn announce_password(resolved: &secrets::Resolved, roster: &std::path::Path) {
    match resolved.source {
        Source::Config => tracing::info!("bot accounts use the password from server.password"),
        Source::Env => tracing::info!(
            "bot accounts use the password from ${}",
            playerbots::config::PASSWORD_ENV
        ),
        Source::SecretsFile => tracing::info!(
            "bot accounts use the generated password in {}",
            secrets::secrets_path(roster).display()
        ),
        Source::Generated => tracing::info!(
            "generated a private password for the bot accounts and saved it to {} (keep that file next to the roster)",
            secrets::secrets_path(roster).display()
        ),
        Source::LegacyDefault => tracing::warn!(
            "the roster {} lists bots but there is no {}: their accounts were created with the old public \
             default password and keep using it. Anyone who knows it can log in as one of the bots. Set \
             server.password (or ${}) and change the accounts' password in the login database, or start over \
             with a new roster (then a private password is generated)",
            roster.display(),
            secrets::secrets_path(roster).display(),
            playerbots::config::PASSWORD_ENV
        ),
    }
}

async fn run(
    config: PathBuf,
    count: Option<u32>,
    duration_secs: Option<u64>,
) -> anyhow::Result<()> {
    let mut cfg = Config::load(&config)?;
    if let Some(n) = count {
        cfg.bots.count = n;
        cfg.validate()?;
    }
    let cfg = Arc::new(cfg);
    let archetypes = Arc::new(Archetypes::builtin()?);

    let paths = WorldPaths {
        compact: cfg.data.compact.clone(),
        server_data: cfg.data.server_data.clone(),
        baked: cfg.data.baked.clone(),
        world: cfg.data.world.clone(),
    };
    let world = tokio::task::spawn_blocking(move || World::load(&paths))
        .await
        .context("the world loader panicked")?
        .with_context(|| {
            format!(
                "loading the world data (data.compact = {})",
                cfg.data.compact.display()
            )
        })?;
    let world = Arc::new(world);

    let roster_path = cfg.bots.roster.clone();
    let roster = Roster::load(&roster_path)?;
    // (decided before the roster grows: a roster that already lists bots has accounts on the
    // login server under the password of the version that created them)
    let resolved = secrets::resolve_from_config(&cfg, &roster_path, !roster.is_empty())?;
    secrets::check_gm_password(&cfg, &resolved)?;
    if cfg.behavior.trade_with_players && cfg.bots.gm_bootstrap {
        tracing::warn!(
            "behavior.trade_with_players is ignored while bots.gm_bootstrap is on: what the bots hold may be GM-made, so real players cannot trade with them"
        );
    }
    if cfg.market.enabled && cfg.market.sell_to_players && cfg.bots.gm_bootstrap {
        tracing::warn!(
            "market.sell_to_players is ignored while bots.gm_bootstrap is on: what the bots hold may be GM-made, so real players cannot buy from them"
        );
    }
    if cfg.pvp.enabled && cfg.pvp.attack_players && cfg.bots.gm_bootstrap {
        tracing::warn!(
            "pvp.attack_players is ignored while bots.gm_bootstrap is on: what the bots hold may be GM-made, so they never attack real players"
        );
    }
    announce_password(&resolved, &roster_path);
    let roster = Arc::new(SharedRoster::new(roster, roster_path));
    let wanted = cfg.bots.count as usize;
    let creatable = {
        let w = world.clone();
        move |race: u8, gender: u8| w.creatable(race, gender)
    };
    let added = ensure_roster(&roster, &cfg, &archetypes, &creatable, wanted)?;
    if added > 0 {
        tracing::info!(added, "new bots in the roster");
    }
    // The roster is on disk before the first bot logs in (the login server creates the accounts
    // then); from here on the saver task writes the changes, a moment after they happen.
    roster.flush_blocking();
    let saver = roster.spawn_saver(playerbots::roster::SAVE_INTERVAL);

    let (sup_tx, sup_rx) = tokio::sync::mpsc::channel::<SupCmd>(16);
    let llm = pb_llm::Llm::from_config(&cfg.llm);
    let issues = Arc::new(Issues::open(&cfg.issues, &cfg.bots.roster));
    let journal_path = cfg.bots.journal_path();
    let journal = Arc::new(playerbots::journal::JournalBook::load(&journal_path)?);
    tracing::info!(
        path = %journal_path.display(),
        bots = journal.ids().len(),
        "journal loaded"
    );
    let journal_saver = journal.spawn_saver(playerbots::journal::SAVE_INTERVAL);
    let hub = Arc::new(
        Hub::new(HubParams {
            world: &world,
            baked_dir: cfg.data.baked.as_deref(),
            supervisor: sup_tx,
            llm,
        })
        .with_issues(issues)
        .with_journal(journal)
        .with_population(Population::new(
            &cfg.population,
            cfg.bots.race_ids(),
            Some(world.clone()),
        )),
    );
    let issue_tasks = hub.start_issues();

    let cancel = CancellationToken::new();
    let (coord_handle, coordinator) = coordinator::channel(hub.clone() as Arc<dyn Dispatch>);
    // (without sites the coordinator never answers a wish for a dungeon)
    let coordinator = if cfg.behavior.dungeons {
        coordinator
            .with_sites(playerbots::bot::party::sites(&world))
            .with_party_relax(cfg.dungeons.relax_after_secs)
    } else {
        coordinator
    };
    // (founding teams for guilds: only when the bots may found guilds at all)
    let coordinator = if cfg.guild.enabled && cfg.guild.found && cfg.guild.max_guilds > 0 {
        coordinator.with_guilds(cfg.guild.party_size, cfg.guild.max_guilds)
    } else {
        coordinator
    };
    let coord_task = tokio::spawn(coordinator.run(cancel.clone()));

    let shared = Arc::new(Shared {
        cfg: cfg.clone(),
        world,
        hub: hub.clone(),
        roster,
        archetypes,
        coord: coord_handle,
        login_gate: tokio::sync::Semaphore::new(LOGIN_PERMITS),
        password: resolved.password,
    });
    let registered = register_slots(&shared);
    tracing::info!(
        bots = registered,
        start = wanted.min(registered),
        login = %cfg.server.login,
        "ready"
    );

    // The web UI binds first so a busy port is an error, not a silent loss.
    let listener = tokio::net::TcpListener::bind(cfg.web.bind)
        .await
        .with_context(|| format!("binding the web UI to {}", cfg.web.bind))?;
    let web = tokio::spawn(pb_web::serve_listener(
        listener,
        hub.clone() as Arc<dyn BotHub>,
        cfg.web.clone(),
        {
            let cancel = cancel.clone();
            async move { cancel.cancelled().await }
        },
    ));

    let ticker = {
        let (hub, cancel) = (hub.clone(), cancel.clone());
        tokio::spawn(async move {
            let mut every = tokio::time::interval(Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = every.tick() => hub.tick(),
                }
            }
        })
    };

    let server_api = serverapi::spawn(&cfg.population, hub.clone(), cancel.clone());

    let supervisor = tokio::spawn(Supervisor::new(shared).run(wanted, sup_rx, cancel.clone()));

    let timer = async {
        match duration_secs {
            Some(s) => tokio::time::sleep(Duration::from_secs(s)).await,
            None => std::future::pending::<()>().await,
        }
    };
    tokio::select! {
        r = tokio::signal::ctrl_c() => {
            if let Err(e) = r {
                tracing::warn!("waiting for Ctrl+C: {e}");
            }
            tracing::info!("Ctrl+C: shutting down");
        }
        () = timer => tracing::info!("duration over: shutting down"),
    }
    cancel.cancel();

    if let Err(e) = supervisor.await {
        tracing::warn!("supervisor: {e}");
    }
    match web.await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => tracing::warn!("web UI: {e:#}"),
        Err(e) => tracing::warn!("web UI task: {e}"),
    }
    let _ = coord_task.await;
    let _ = ticker.await;
    if let Some(t) = server_api {
        let _ = t.await;
    }
    // (the bots are gone: whatever they changed last is written now)
    saver.stop().await;
    journal_saver.stop().await;
    issue_tasks.stop().await;
    tracing::info!("bye");
    Ok(())
}
