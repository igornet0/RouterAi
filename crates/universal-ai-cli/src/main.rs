//! `ai` CLI — providers, models, accounts, keys, balance, usage, health, stats.

use std::process::ExitCode;
use std::time::Duration;

use clap::{Parser, Subcommand};
use secrecy::SecretString;
use tracing_subscriber::EnvFilter;
use universal_ai::{
    AccountId, AiClient, DeepSeek, MemorySecretStore, OpenAI, OpenAICompatible, ProviderId,
};

#[derive(Parser, Debug)]
#[command(name = "ai", about = "universal-ai CLI", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// List configured providers (from env).
    Providers,
    /// List / discover models.
    Models {
        /// Refresh from provider APIs.
        #[arg(long)]
        refresh: bool,
    },
    /// Account operations.
    Accounts {
        #[command(subcommand)]
        action: AccountCmd,
    },
    /// API key operations.
    Keys {
        #[command(subcommand)]
        action: KeyCmd,
    },
    /// Show balances.
    Balance,
    /// Show usage.
    Usage {
        /// Restrict to today (UTC).
        #[arg(long)]
        today: bool,
    },
    /// Show pricing registry entries (example prices if loaded).
    Pricing,
    /// Health check providers.
    Health,
    /// Aggregated stats.
    Stats {
        #[arg(long)]
        today: bool,
    },
    /// Quick chat (requires DEEPSEEK_API_KEY or OPENAI_API_KEY).
    Chat {
        /// Model id.
        #[arg(long, default_value = "deepseek-chat")]
        model: String,
        /// User message.
        message: String,
    },
}

#[derive(Subcommand, Debug)]
enum AccountCmd {
    /// List accounts.
    List,
    /// Add an account.
    Add { provider: String, name: String },
}

#[derive(Subcommand, Debug)]
enum KeyCmd {
    /// List key metadata (secrets never printed).
    List,
    /// Add a key from env or flag.
    Add {
        provider: String,
        account: String,
        #[arg(long, env = "AI_API_KEY")]
        secret: String,
        #[arg(long)]
        name: Option<String>,
    },
}

#[tokio::main]
async fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("info".parse().unwrap()))
        .init();

    let cli = Cli::parse();
    match run(cli).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("error: {err}");
            ExitCode::FAILURE
        }
    }
}

async fn build_client() -> universal_ai::AiResult<AiClient> {
    let mut builder = AiClient::builder()
        .secret_store(std::sync::Arc::new(MemorySecretStore::new()))
        .with_example_prices()
        .fallback(true);

    let mut any = false;
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY") {
        if !key.is_empty() {
            builder = builder.provider(DeepSeek::new(SecretString::new(key.into()))?);
            any = true;
        }
    }
    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        if !key.is_empty() {
            builder = builder.provider(OpenAI::new(SecretString::new(key.into()))?);
            any = true;
        }
    }
    if let Ok(base) = std::env::var("OPENAI_COMPATIBLE_BASE_URL") {
        let key = std::env::var("OPENAI_COMPATIBLE_API_KEY").unwrap_or_else(|_| "local".into());
        builder = builder.provider(
            OpenAICompatible::builder()
                .base_url(base)
                .api_key(SecretString::new(key.into()))
                .build()?,
        );
        any = true;
    }

    if !any {
        // Allow offline commands with a placeholder compatible provider pointing nowhere.
        builder = builder.provider(
            OpenAICompatible::builder()
                .base_url("http://127.0.0.1:9/v1")
                .api_key(SecretString::new("offline".into()))
                .build()?,
        );
    }

    builder.build()
}

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    let client = build_client().await?;

    match cli.command {
        Commands::Providers => {
            println!("{:<14} {:<12} Capabilities", "Provider", "Chat");
            for p in client.provider_summaries() {
                let caps = &p.capabilities;
                println!(
                    "{:<14} {:<12} stream={} balance={} models={}",
                    p.id, caps.chat, caps.streaming, caps.balance, caps.model_list
                );
            }
        }
        Commands::Models { refresh } => {
            if refresh {
                let _ = client.discover_models().await;
            }
            let models = client.models().list().await?;
            if models.is_empty() {
                println!("(no models in registry — try `ai models --refresh`)");
            }
            for m in models {
                println!("{:<12} {}", m.provider, m.id);
            }
        }
        Commands::Accounts { action } => match action {
            AccountCmd::List => {
                for a in client.accounts().list_accounts().await {
                    println!("{:<24} {:<12} {}", a.id, a.provider, a.name);
                }
            }
            AccountCmd::Add { provider, name } => {
                let a = client
                    .accounts()
                    .add_account(ProviderId::new(provider), name)
                    .await?;
                println!("created account {}", a.id);
            }
        },
        Commands::Keys { action } => match action {
            KeyCmd::List => {
                println!(
                    "{:<24} {:<12} {:<24} {:?}",
                    "KeyId", "Provider", "Account", "Status"
                );
                for k in client.keys().list_keys().await {
                    println!(
                        "{:<24} {:<12} {:<24} {:?}",
                        k.id, k.provider, k.account_id, k.status
                    );
                }
            }
            KeyCmd::Add {
                provider,
                account,
                secret,
                name,
            } => {
                let info = client
                    .keys()
                    .add_key(universal_ai::account::AddKeyRequest {
                        provider: ProviderId::new(provider),
                        account_id: AccountId::new(account),
                        secret: SecretString::new(secret.into()),
                        name,
                        base_url: None,
                    })
                    .await?;
                println!(
                    "added key {} (secret stored securely, not printed)",
                    info.id
                );
            }
        },
        Commands::Balance => {
            println!(
                "{:<12} {:<12} {:<12} Updated",
                "Provider", "Currency", "Balance"
            );
            for p in client.provider_summaries() {
                if !p.capabilities.supports(universal_ai::Capability::Balance) {
                    continue;
                }
                match client.balance_for(&p.id).await {
                    Ok(Some(b)) => {
                        let ago = (chrono::Utc::now() - b.updated_at).num_seconds().max(0);
                        println!(
                            "{:<12} {:<12} ${:<11} {} sec ago",
                            b.provider, b.currency, b.total, ago
                        );
                    }
                    Ok(None) => println!("{:<12} (no balance reported)", p.id),
                    Err(err) => println!("{:<12} error: {err}", p.id),
                }
            }
        }
        Commands::Usage { today } | Commands::Stats { today } => {
            let stats = if today {
                client.stats().today().await
            } else {
                client.stats().all().await
            };
            println!("Requests       {}", stats.requests);
            println!("Input tokens   {}", format_tokens(stats.input_tokens));
            println!("Output tokens  {}", format_tokens(stats.output_tokens));
            println!("Total cost     ${}", stats.total_cost);
            println!("Avg latency    {:.1} ms", stats.average_latency_ms);
        }
        Commands::Pricing => {
            // Example prices loaded by CLI builder.
            println!("Pricing is runtime-updatable via client.pricing().upsert(...)");
            println!("Example prices loaded for deepseek-chat and gpt-4o-mini when CLI starts.");
        }
        Commands::Health => {
            for p in client.provider_summaries() {
                let status = client.check_provider_health(&p.id).await?;
                println!(
                    "{:<14} healthy={} latency={:?} err={:?}",
                    p.id, status.healthy, status.latency_ms, status.error
                );
            }
        }
        Commands::Chat { model, message } => {
            let response = client.chat().model(model).message(message).send().await?;
            println!("{}", response.text());
            if let Some(u) = response.usage() {
                eprintln!(
                    "usage: prompt={} completion={}",
                    u.prompt_tokens, u.completion_tokens
                );
            }
            if let Some(c) = response.cost() {
                eprintln!("cost: {c}");
            }
        }
    }

    // Keep unused import quiet for Duration in case we add monitor later.
    let _ = Duration::from_secs(1);
    Ok(())
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.2}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.2}K", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}
