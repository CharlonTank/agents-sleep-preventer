// Linux: server mode, for a machine that never turns off. No menu bar,
// dictation or sleep prevention: Telegram, the RAM and disk watch, and the
// workshop in tmux, from the CLI and a systemd user service (`asp serve`).

mod claude_profiles;
mod logging;
mod monitor;
mod telegram;
mod workshop;

use anyhow::{bail, Context, Result};
use claude_profiles::claude_config_dirs;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::process::Command;

/// A process, as the macOS reader gives it.
struct ProcessInfo {
    pid: u32,
    ppid: u32,
    /// argv[0] whole.
    arg0: String,
    /// argv joined with spaces, on one line, as `ps -o args` shows it.
    args: String,
}

/// Arguments keep the newlines and tabs of inline scripts; show them on one line.
fn one_line(text: &str) -> String {
    text.chars().map(|c| if c.is_control() { ' ' } else { c }).collect()
}

fn read_process(pid: u32) -> Option<ProcessInfo> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let (open, close) = (stat.find('(')?, stat.rfind(')')?);
    let ppid = stat[close + 1..].split_whitespace().nth(1)?.parse().ok()?;
    let raw = std::fs::read(format!("/proc/{pid}/cmdline")).unwrap_or_default();
    let mut argv: Vec<String> = raw
        .split(|byte| *byte == 0)
        .filter(|arg| !arg.is_empty())
        .map(|arg| String::from_utf8_lossy(arg).into_owned())
        .collect();
    if argv.is_empty() {
        argv.push(stat[open + 1..close].to_string()); // a kernel thread or a zombie
    }
    Some(ProcessInfo {
        pid,
        ppid,
        arg0: one_line(&argv[0]),
        args: one_line(&argv.join(" ")),
    })
}

/// Every process, from /proc.
fn load_process_table() -> Vec<ProcessInfo> {
    let Ok(entries) = std::fs::read_dir("/proc") else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| entry.file_name().to_str()?.parse::<u32>().ok())
        .filter_map(read_process)
        .collect()
}

fn get_process_cwd(pid: u32) -> Option<String> {
    std::fs::read_link(format!("/proc/{pid}/cwd"))
        .ok()
        .map(|path| path.display().to_string())
}

#[derive(Parser)]
#[command(name = "asp")]
#[command(about = "Control your coding agent sessions from Telegram, and watch their RAM and disk")]
#[command(version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Talk with your coding agent sessions from Telegram, through your own bot
    Telegram {
        #[command(subcommand)]
        command: telegram::TelegramCommand,
    },
    /// Watch RAM and disk, and name the Claude Code sessions behind a drift
    Monitor {
        #[command(subcommand)]
        command: monitor::MonitorCommand,
    },
    /// Open your named Claude Code sessions in tmux ([workshop] in config.toml)
    Workshop {
        /// Claude Code profile ([workshop.profiles]); default: the last used
        profile: Option<String>,
        /// Even if Claude already runs in those folders
        #[arg(long)]
        force: bool,
        /// Only the tab holding this session, in a new window
        #[arg(long)]
        only: Option<String>,
    },
    #[command(hide = true)]
    WorkshopPilot { plan: String },
    /// Run the RAM and disk watch in the foreground (what the service runs)
    Serve,
    /// The systemd user service that runs `asp serve`
    Service {
        #[command(subcommand)]
        command: ServiceCommand,
    },
}

#[derive(Subcommand)]
enum ServiceCommand {
    /// Install, enable and start it (and keep it running after logout)
    Install,
    /// Stop, disable and remove it
    Uninstall,
}

const UNIT: &str = "asp.service";

fn unit_file() -> Result<PathBuf> {
    Ok(dirs::config_dir()
        .context("no config directory")?
        .join("systemd/user")
        .join(UNIT))
}

fn systemctl(args: &[&str]) -> Result<()> {
    let status = Command::new("systemctl")
        .arg("--user")
        .args(args)
        .status()
        .context("systemctl")?;
    if !status.success() {
        bail!("systemctl --user {} failed", args.join(" "));
    }
    Ok(())
}

fn service(command: ServiceCommand) -> Result<()> {
    let file = unit_file()?;
    match command {
        ServiceCommand::Install => {
            let exe = std::env::current_exe().context("current executable")?;
            if let Some(parent) = file.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(
                &file,
                format!(
                    "[Unit]\nDescription=Agents Sleep Preventer: RAM and disk watch for coding agent sessions\nAfter=network-online.target\n\n[Service]\nExecStart={} serve\nRestart=always\nRestartSec=10\n\n[Install]\nWantedBy=default.target\n",
                    exe.display()
                ),
            )?;
            systemctl(&["daemon-reload"])?;
            systemctl(&["enable", "--now", UNIT])?;
            // Without linger, user services stop at logout: a server needs it.
            let user = std::env::var("USER").unwrap_or_default();
            let linger = Command::new("loginctl").args(["enable-linger", &user]).status();
            println!("Service installed and started: {}", file.display());
            if !linger.is_ok_and(|status| status.success()) {
                println!("To keep it running after logout: sudo loginctl enable-linger {user}");
            }
        }
        ServiceCommand::Uninstall => {
            let _ = systemctl(&["disable", "--now", UNIT]);
            let _ = std::fs::remove_file(&file);
            let _ = systemctl(&["daemon-reload"]);
            println!("Service removed.");
        }
    }
    Ok(())
}

fn main() -> Result<()> {
    // A command piped into `head` ends quietly, like any Unix tool.
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }
    match Cli::parse().command {
        Commands::Telegram { command } => std::process::exit(telegram::run(command)),
        Commands::Monitor { command } => std::process::exit(monitor::run(command)),
        Commands::Workshop { profile, force, only } => {
            workshop::open(profile.as_deref(), force, only.as_deref())?
        }
        Commands::WorkshopPilot { plan } => workshop::pilot(serde_json::from_str(&plan)?),
        Commands::Serve => {
            logging::init();
            monitor::spawn();
            loop {
                std::thread::park();
            }
        }
        Commands::Service { command } => service(command)?,
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn this_process_reads_like_ps_shows_it() {
        let me = read_process(std::process::id()).unwrap();
        assert_eq!(me.ppid, std::os::unix::process::parent_id());
        assert!(me.args.contains(&me.arg0));
        assert!(get_process_cwd(me.pid).is_some());
        assert!(load_process_table().iter().any(|p| p.pid == me.pid));
    }
}
