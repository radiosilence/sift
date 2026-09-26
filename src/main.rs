//! `sift` — tag and file music. Reads your beets config.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sift::{Config, Importer, Outcome};

#[derive(Parser)]
#[command(
    name = "sift",
    version,
    about = "Match music against MusicBrainz, tag it, and file it into a library"
)]
struct Cli {
    /// Config file. Defaults to the beets config: $BEETSDIR/config.yaml or
    /// ~/.config/beets/config.yaml.
    #[arg(short, long, global = true)]
    config: Option<PathBuf>,
    /// Library directory, overriding the config's `directory`.
    #[arg(short = 'd', long, global = true)]
    directory: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Import albums: one per directory given.
    Import {
        paths: Vec<PathBuf>,
        /// Apply this MusicBrainz release, whatever the match distance.
        #[arg(long = "search-id")]
        search_id: Option<String>,
        /// File by the files' own tags without MusicBrainz, as beets' `-A`
        /// does; refused when the tags do not describe one album.
        #[arg(short = 'A', long = "as-is", alias = "noautotag")]
        as_is: bool,
        /// Copy rather than move, whatever the config says.
        #[arg(short = 'c', long)]
        copy: bool,
        /// Move rather than copy, whatever the config says.
        #[arg(short = 'm', long)]
        r#move: bool,
        /// Accepted for beets compatibility; sift never prompts.
        #[arg(short, long)]
        quiet: bool,
        /// Append a line per album to this file.
        #[arg(short = 'l', long)]
        log: Option<PathBuf>,
    },
    /// Show the candidates for a directory without changing anything.
    Match { path: PathBuf },
}

#[tokio::main]
async fn main() -> ExitCode {
    match run().await {
        Ok(code) => code,
        Err(e) => {
            eprintln!("sift: {e:#}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> anyhow::Result<ExitCode> {
    let cli = Cli::parse();
    let path = cli.config.clone().or_else(Config::default_path);
    let mut cfg = match &path {
        Some(p) => Config::load(p)?,
        None => Config::default(),
    };
    if let Some(d) = cli.directory {
        cfg.directory = d;
    }
    anyhow::ensure!(
        !cfg.directory.as_os_str().is_empty(),
        "no library directory: pass --directory or set `directory` in the config"
    );

    match cli.command {
        Command::Import {
            paths,
            search_id,
            as_is,
            copy,
            r#move,
            quiet: _,
            log,
        } => {
            if copy {
                cfg.move_files = false;
            }
            if r#move {
                cfg.move_files = true;
            }
            let importer = Importer::new(cfg);
            let mut failed = false;
            for dir in paths {
                let outcome = if as_is {
                    importer.import_as_is(&dir).await
                } else {
                    importer.import(&dir, search_id.as_deref()).await
                };
                let line = match outcome {
                    Ok(Outcome::Imported {
                        dir: dest,
                        release: Some(release),
                        ..
                    }) => {
                        println!(
                            "imported  {} — {}  →  {}",
                            release.artist,
                            release.title,
                            dest.display()
                        );
                        format!("import {} {}", release.id, dir.display())
                    }
                    Ok(Outcome::Imported {
                        dir: dest,
                        release: None,
                        ..
                    }) => {
                        println!("imported  as-is  →  {}", dest.display());
                        format!("import as-is {}", dir.display())
                    }
                    Ok(Outcome::Review {
                        reason, candidates, ..
                    }) => {
                        failed = true;
                        println!("skipped   {}: {reason}", dir.display());
                        for c in candidates.iter().take(5) {
                            println!(
                                "          {:.3}  {}  {} — {} ({}{})",
                                c.distance,
                                c.id,
                                c.artist,
                                c.title,
                                c.date.as_deref().unwrap_or("?"),
                                c.country
                                    .as_deref()
                                    .map(|x| format!(", {x}"))
                                    .unwrap_or_default()
                            );
                        }
                        format!("skip {}", dir.display())
                    }
                    Err(e) => {
                        failed = true;
                        eprintln!("failed    {}: {e}", dir.display());
                        format!("error {} {e}", dir.display())
                    }
                };
                if let Some(log) = &log {
                    use std::io::Write;
                    let mut f = std::fs::OpenOptions::new()
                        .create(true)
                        .append(true)
                        .open(log)?;
                    writeln!(f, "{line}")?;
                }
            }
            Ok(if failed {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Match { path } => {
            cfg.strong_threshold = -1.0;
            let importer = Importer::new(cfg);
            match importer.import(&path, None).await? {
                Outcome::Review {
                    candidates, log, ..
                } => {
                    eprint!("{log}");
                    for c in candidates {
                        println!(
                            "{:.3}  {}  {} — {} ({}{}) {} tracks, {} missing, {} extra",
                            c.distance,
                            c.id,
                            c.artist,
                            c.title,
                            c.date.as_deref().unwrap_or("?"),
                            c.country
                                .as_deref()
                                .map(|x| format!(", {x}"))
                                .unwrap_or_default(),
                            c.tracks,
                            c.missing,
                            c.extra
                        );
                    }
                }
                Outcome::Imported { .. } => unreachable!("a negative threshold never auto-applies"),
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}
