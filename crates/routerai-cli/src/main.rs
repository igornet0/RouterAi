//! `routerai` CLI.

use std::process::ExitCode;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use routerai::{
    published_agent, Action, AgentId, Event, Handler, RouterRuntime, Schedule,
};
use serde_json::json;
use tracing_subscriber::EnvFilter;

#[derive(Parser, Debug)]
#[command(name = "routerai", about = "RouterAi agent runtime CLI", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand, Debug)]
enum Commands {
    /// Runtime health checks.
    Doctor,
    /// Event commands.
    Events {
        #[command(subcommand)]
        action: EventCmd,
    },
    /// Handler commands.
    Handlers {
        #[command(subcommand)]
        action: HandlerCmd,
    },
    /// Agent commands.
    Agents {
        #[command(subcommand)]
        action: AgentCmd,
    },
    /// Run commands.
    Runs {
        #[command(subcommand)]
        action: RunCmd,
    },
    /// Tool commands.
    Tools {
        #[command(subcommand)]
        action: ToolCmd,
    },
    /// Schedule tick (once).
    Schedules {
        #[command(subcommand)]
        action: ScheduleCmd,
    },
}

#[derive(Subcommand, Debug)]
enum EventCmd {
    /// List recent events.
    List,
    /// Emit an event.
    Emit {
        /// Event type.
        event_type: String,
        /// JSON payload.
        #[arg(long, default_value = "{}")]
        payload: String,
        /// Source.
        #[arg(long, default_value = "cli")]
        source: String,
    },
}

#[derive(Subcommand, Debug)]
enum HandlerCmd {
    List,
    /// Create a simple agent-run handler (demo).
    CreateSales {
        /// Agent id to invoke.
        agent_id: String,
    },
}

#[derive(Subcommand, Debug)]
enum AgentCmd {
    List,
    /// Create a demo published agent.
    Create {
        name: String,
        #[arg(long, default_value = "You are a helpful agent.")]
        instructions: String,
    },
    /// Run an agent with a message.
    Run {
        agent_id: String,
        message: String,
    },
    /// Run a quick test case.
    Test {
        agent_id: String,
        message: String,
        #[arg(long)]
        must_contain: Vec<String>,
    },
}

#[derive(Subcommand, Debug)]
enum RunCmd {
    List,
}

#[derive(Subcommand, Debug)]
enum ToolCmd {
    List,
}

#[derive(Subcommand, Debug)]
enum ScheduleCmd {
    List,
    /// Create interval schedule for an agent.
    Create {
        name: String,
        agent_id: String,
        #[arg(long, default_value_t = 3600)]
        every_secs: u64,
    },
    Tick,
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

async fn run(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    // Process-local runtime for CLI demos. Server holds the long-lived instance.
    let rt = Arc::new(RouterRuntime::builder().build().await?);

    match cli.command {
        Commands::Doctor => {
            let d = rt.doctor().await;
            println!("RouterAi runtime       {}", ok(d.runtime_ok));
            println!("Kill switch            {}", if d.kill_switch { "ON" } else { "off" });
            println!("Event bus              {}", ok(d.event_bus_ok));
            println!("AiClient               {}", ok(d.ai_configured));
            println!("Agents                 {}", d.agents);
            println!("Handlers               {}", d.handlers);
            println!("Tools                  {}", d.tools);
            println!("Schedules              {}", d.schedules);
        }
        Commands::Events { action } => match action {
            EventCmd::List => {
                for e in rt.events().list(20).await {
                    println!("{:<28} {:<12} {}", e.event_type, e.source, e.id);
                }
            }
            EventCmd::Emit {
                event_type,
                payload,
                source,
            } => {
                let payload: serde_json::Value = serde_json::from_str(&payload)?;
                let event = Event::new(event_type, source, payload);
                let runs = rt.emit(event.clone()).await?;
                println!("emitted {}", event.id);
                println!("triggered {} run(s)", runs.len());
                for r in runs {
                    println!("  {} status={:?} cost={}", r.id, r.status, r.cost);
                }
            }
        },
        Commands::Handlers { action } => match action {
            HandlerCmd::List => {
                for h in rt.handlers().list().await {
                    println!(
                        "{:<24} {:<20} enabled={} trigger={}",
                        h.id, h.name, h.enabled, h.trigger.event
                    );
                }
            }
            HandlerCmd::CreateSales { agent_id } => {
                let h = Handler::agent_on_event(
                    "Sales handler",
                    "telegram.message.received",
                    AgentId::from_string(agent_id),
                );
                rt.upsert_handler(h.clone()).await?;
                println!("created handler {}", h.id);
            }
        },
        Commands::Agents { action } => match action {
            AgentCmd::List => {
                for a in rt.agents().list().await {
                    println!(
                        "{:<24} {:<20} {:?} v{} model={}",
                        a.id, a.name, a.status, a.version, a.model.model
                    );
                }
            }
            AgentCmd::Create { name, instructions } => {
                let a = published_agent(&name, &instructions);
                let a = rt.upsert_agent(a).await?;
                println!("created agent {}", a.id);
            }
            AgentCmd::Run { agent_id, message } => {
                let run = rt
                    .start_run(
                        &AgentId::from_string(agent_id),
                        json!({ "message": message }),
                        None,
                    )
                    .await?;
                println!("run {} {:?}", run.id, run.status);
                if let Some(out) = run.output {
                    println!("{out}");
                }
            }
            AgentCmd::Test {
                agent_id,
                message,
                must_contain,
            } => {
                let needles: Vec<&str> = must_contain.iter().map(|s| s.as_str()).collect();
                let case = routerai::text_case(
                    "cli-test",
                    AgentId::from_string(agent_id),
                    message,
                    &needles,
                    &[],
                );
                let (run, report) = rt.run_test(&case).await?;
                println!("run {} {:?}", run.id, run.status);
                println!(
                    "evaluation passed={} cost={} latency_ms={}",
                    report.passed, report.cost, report.latency_ms
                );
                for a in report.assertions {
                    println!("  [{}] {}", if a.passed { "✓" } else { "✗" }, a.message);
                }
            }
        },
        Commands::Runs { action } => match action {
            RunCmd::List => {
                for r in rt.store().list_runs(20).await? {
                    println!(
                        "{:<24} {:<24} {:?} ${}",
                        r.id, r.agent_id, r.status, r.cost
                    );
                }
            }
        },
        Commands::Tools { action } => match action {
            ToolCmd::List => {
                for t in rt.tools().registry().list().await {
                    println!("{:<20} {}", t.id, t.description);
                }
            }
        },
        Commands::Schedules { action } => match action {
            ScheduleCmd::List => {
                for s in rt.scheduler().list().await {
                    println!("{:<24} {:<20} enabled={}", s.id, s.name, s.enabled);
                }
            }
            ScheduleCmd::Create {
                name,
                agent_id,
                every_secs,
            } => {
                let s = Schedule::every(
                    name,
                    every_secs,
                    Action::AgentRun {
                        agent_id: AgentId::from_string(agent_id),
                    },
                );
                rt.upsert_schedule(s.clone()).await?;
                println!("created schedule {}", s.id);
            }
            ScheduleCmd::Tick => {
                let runs = rt.tick_schedules().await?;
                println!("fired {} run(s)", runs.len());
            }
        },
    }
    Ok(())
}

fn ok(v: bool) -> &'static str {
    if v {
        "✓"
    } else {
        "✗"
    }
}
