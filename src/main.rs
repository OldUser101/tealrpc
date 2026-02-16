use anyhow::{Result, bail};
use clap::{
    CommandFactory, FromArgMatches, Parser,
    builder::{
        Styles,
        styling::{AnsiColor, Effects},
    },
};
use discord_presence::{
    Client, Event,
    models::{ActivityType, DisplayType},
};
use jacquard::{
    client::{AgentSessionExt, BasicClient},
    identity::JacquardResolver,
    prelude::{IdentityResolver, PublicResolver},
    types::{did::Did, string::Handle},
    url::Url,
    xrpc::{SubscriptionClient, TungsteniteSubscriptionClient},
};
use jacquard_api::com_atproto::sync::subscribe_repos::{SubscribeRepos, SubscribeReposMessage};
use jacquard_api::fm_teal::alpha::actor::status as fm_teal_status;
use n0_future::StreamExt;
use owo_colors::OwoColorize;

const DISCORD_APP_ID: u64 = 1472702474166079620;

fn args_styles() -> Styles {
    Styles::styled()
        .header(AnsiColor::BrightGreen.on_default().effects(Effects::BOLD))
        .usage(AnsiColor::BrightGreen.on_default().effects(Effects::BOLD))
        .literal(AnsiColor::BrightCyan.on_default().effects(Effects::BOLD))
        .placeholder(AnsiColor::BrightYellow.on_default())
        .valid(AnsiColor::BrightGreen.on_default())
        .invalid(AnsiColor::BrightRed.on_default())
}

#[derive(Parser, Debug)]
struct Args {
    /// Handle or DID to subscribe to
    ident: String,
}

fn get_command() -> clap::Command {
    Args::command().styles(args_styles())
}

fn normalize_url(input: &str) -> Result<Url> {
    let without_scheme = input
        .trim_start_matches("https://")
        .trim_start_matches("http://")
        .trim_start_matches("wss://")
        .trim_start_matches("ws://");

    Ok(Url::parse(&format!("wss://{}", without_scheme))?)
}

struct Resolver {
    resolver: JacquardResolver,
}

impl Resolver {
    fn new() -> Self {
        Self {
            resolver: PublicResolver::default(),
        }
    }

    async fn resolve_did(&self, ident: &str) -> Result<Did<'_>> {
        if let Ok(did) = ident.parse() {
            return Ok(did);
        }

        let handle = Handle::new(ident)?;
        let did = self.resolver.resolve_handle(&handle).await?;
        Ok(did)
    }

    async fn get_pds(&self, did: &Did<'_>) -> Result<Url> {
        let pds = self.resolver.pds_for_did(did).await?;
        Ok(pds)
    }
}

struct Status {
    track_name: String,
    artists: Vec<String>,
}

impl Status {
    fn artists(&self) -> String {
        let mut artists_str = String::new();

        for i in 0..self.artists.len() {
            artists_str += &self.artists[i];

            if i != self.artists.len() - 1 {
                artists_str += ", ";
            }
        }

        artists_str
    }
}

const STATUS_PATH: &str = "fm.teal.alpha.actor.status/self";

fn get_status_endpoint(did: String) -> String {
    format!("at://{}/{}", did, STATUS_PATH)
}

async fn get_status(did: &Did<'_>) -> Result<Option<Status>> {
    let endpoint = get_status_endpoint(did.to_string());
    let uri = fm_teal_status::Status::uri(&endpoint)?;

    let agent = BasicClient::unauthenticated();
    let response = agent
        .get_record::<fm_teal_status::StatusRecord>(&uri)
        .await?;

    let status_rec = response.into_output()?.value;

    if status_rec.item.track_name.is_empty() {
        return Ok(None);
    }

    Ok(Some(Status {
        track_name: status_rec.item.track_name.to_string(),
        artists: status_rec
            .item
            .artists
            .iter()
            .map(|a| a.artist_name.to_string())
            .collect(),
    }))
}

async fn _main() -> Result<()> {
    let mut matches = get_command().get_matches();
    let args = Args::from_arg_matches_mut(&mut matches)?;

    let listener = Resolver::new();

    let did = listener.resolve_did(&args.ident).await?;
    let pds = listener.get_pds(&did).await?.to_string();
    let pds = normalize_url(&pds)?;

    println!("{}: {}", "did".magenta().bold(), did);
    println!("{}: {}", "pds".magenta().bold(), pds);

    let status = get_status(&did).await?;

    let mut drpc = Client::new(DISCORD_APP_ID);
    drpc.start();
    drpc.block_until_event(Event::Ready)?;

    if !Client::is_ready() {
        bail!("discord rpc client not ready");
    } else {
        println!("{}: discord rpc ready", "info".blue().bold());
    }

    if let Some(status) = status {
        println!("{}: set initial playing status", "info".blue().bold());
        drpc.set_activity(|act| {
            act.state(format!("{}, {}", status.track_name, status.artists()))
                .activity_type(ActivityType::Listening)
                .status_display(DisplayType::State)
        })?;
    }

    let client = TungsteniteSubscriptionClient::from_base_uri(pds);
    let params = SubscribeRepos::new().build();
    let stream = client.subscribe(&params).await?;

    let (tx, mut rx) = tokio::sync::oneshot::channel();
    tokio::spawn(async move {
        tokio::signal::ctrl_c().await.ok();
        let _ = tx.send(());
    });

    let (_sink, mut messages) = stream.into_stream();

    loop {
        tokio::select! {
            Some(result) = messages.next() => {
                match result {
                    Ok(msg) => {
                        if let SubscribeReposMessage::Commit(commit) = msg && commit.repo == did {
                            let mut status_changed = false;
                            for op in commit.ops {
                                if op.path == STATUS_PATH {
                                    status_changed = true;
                                }
                            }

                            if status_changed {
                                match get_status(&did).await {
                                    Ok(Some(status)) => {
                                        println!("{}: updated playing status", "info".blue().bold());
                                        drpc.set_activity(|act| {
                                            act.state(format!(
                                                    "{}, {}",
                                                    status.track_name,
                                                    status.artists()))
                                                .activity_type(ActivityType::Listening)
                                                .status_display(DisplayType::State)
                                        })?;
                                    },
                                    Ok(None) => {
                                        println!("{}: cleared playing status", "info".blue().bold());
                                        drpc.clear_activity()?;
                                    }
                                    Err(e) => println!("{}: {}", "error".red().bold(), e),
                                }

                            }
                        }
                    },
                    Err(e) => {
                        println!("{}: {}", "error".red().bold(), e);
                    },
                }
            },
            _ = &mut rx => {
                println!("{}: shutting down", "info".blue().bold());
                break;
            }
        }
    }

    drpc.shutdown()?;

    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = _main().await {
        println!("{}: {}", "error".red().bold(), e);
        std::process::exit(1);
    }
}
