//! `sift` — tag and file music. Reads your beets config.

use std::path::PathBuf;
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use sift::library::{self, Library, Query};
use sift::manage;
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
    /// Library index. Defaults to sift/library.db in the user data
    /// directory; never beets' own library.db, whose schema is beets'.
    #[arg(long, global = true)]
    index: Option<PathBuf>,
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
    /// Bring the library index in line with the files in the library.
    Update,
    /// Re-file matching albums where the current path rules put them.
    Move {
        query: Vec<String>,
        /// Show what would move, and move nothing.
        #[arg(short, long)]
        pretend: bool,
        /// List every file, not only each album.
        #[arg(short, long)]
        verbose: bool,
    },
    /// Change fields on matching files, beets-style: `sift modify QUERY
    /// field=value field!`. Albums whose path the change affects are
    /// re-filed.
    Modify {
        args: Vec<String>,
        /// Every file of each matching album.
        #[arg(short, long)]
        album: bool,
        /// List the files that would change, and change nothing.
        #[arg(short, long)]
        pretend: bool,
    },
    /// Find albums held more than once and say which copy to keep.
    #[command(alias = "dup")]
    Duplicates {
        query: Vec<String>,
        /// Move every copy but the best into this directory, at its path
        /// relative to the library. Nothing is deleted.
        #[arg(long)]
        bin: Option<PathBuf>,
    },
    /// List items (or albums, with -a) matching a beets query.
    #[command(alias = "list")]
    Ls {
        query: Vec<String>,
        #[arg(short, long)]
        album: bool,
        /// Print paths rather than names.
        #[arg(short, long)]
        path: bool,
        /// Output format, beets-style: `$artist - $title`.
        #[arg(short, long)]
        format: Option<String>,
    },
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

    let index = cli
        .index
        .clone()
        .or_else(|| dirs::data_dir().map(|d| d.join("sift").join("library.db")))
        .ok_or_else(|| anyhow::anyhow!("no data directory: pass --index"))?;

    match cli.command {
        Command::Update => {
            let mut lib = Library::open(&index)?;
            let r = lib.update(&cfg.directory)?;
            for (_, e) in &r.failed {
                eprintln!("unreadable  {e}");
            }
            println!(
                "{} added, {} changed, {} removed, {} unchanged, {} unreadable",
                r.added,
                r.changed,
                r.removed,
                r.unchanged,
                r.failed.len()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Move {
            query,
            pretend,
            verbose,
        } => {
            let mut lib = Library::open(&index)?;
            lib.update(&cfg.directory)?;
            let (mut moved, mut refused) = (0, 0);
            for album in lib.albums(&Query::parse(&query)?)? {
                match manage::plan_move(&cfg, &album) {
                    manage::Plan::InPlace => {}
                    manage::Plan::Refused(why) => {
                        refused += 1;
                        eprintln!("left  {}: {why}", album.dir.display());
                    }
                    manage::Plan::Moves(moves) => {
                        let to = moves
                            .first()
                            .and_then(|m| m.to.parent())
                            .unwrap_or(&album.dir)
                            .to_path_buf();
                        println!("move  {}  →  {}", album.dir.display(), to.display());
                        if verbose {
                            for m in &moves {
                                println!("        {}  →  {}", m.from.display(), m.to.display());
                            }
                        }
                        if !pretend {
                            manage::execute(&mut lib, &album, &moves).await?;
                        }
                        moved += 1;
                    }
                }
            }
            let verb = if pretend { "would move" } else { "moved" };
            println!("{moved} albums {verb}, {refused} left where they are");
            Ok(ExitCode::SUCCESS)
        }
        Command::Modify {
            args,
            album,
            pretend,
        } => {
            let (query, changes) = manage::split_modify_args(&args).map_err(anyhow::Error::msg)?;
            anyhow::ensure!(
                !changes.is_empty(),
                "nothing to change: give field=value or field!"
            );
            anyhow::ensure!(
                !query.is_empty(),
                "give a query; modifying the whole library takes an explicit \"\""
            );
            let mut lib = Library::open(&index)?;
            lib.update(&cfg.directory)?;
            let r = manage::modify(
                &cfg,
                &mut lib,
                &Query::parse(&query)?,
                album,
                &changes,
                pretend,
            )
            .await?;
            let verb = if pretend { "would change" } else { "changed" };
            for f in &r.files {
                println!("{verb}  {}", f.display());
            }
            for (from, to) in &r.moved {
                println!("moved  {}  →  {}", from.display(), to.display());
            }
            for (dir, why) in &r.left {
                eprintln!("left   {}: {why}", dir.display());
            }
            println!(
                "{} files {verb}, {} albums re-filed",
                r.files.len(),
                r.moved.len()
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Duplicates { query, bin } => {
            let mut lib = Library::open(&index)?;
            lib.update(&cfg.directory)?;
            let albums = lib.albums(&Query::parse(&query)?)?;
            let dupes = manage::duplicates(&cfg, &albums);
            for d in &dupes {
                println!("keep  {}", d.keep.dir.display());
                for (other, why) in &d.others {
                    match &bin {
                        Some(bin) => {
                            let dest = manage::bin(&mut lib, &cfg.directory, bin, other).await?;
                            println!(
                                "  bin {}  ({why})  →  {}",
                                other.dir.display(),
                                dest.display()
                            );
                        }
                        None => println!("  dup {}  ({why})", other.dir.display()),
                    }
                }
            }
            println!("{} albums held more than once", dupes.len());
            Ok(ExitCode::SUCCESS)
        }
        Command::Ls {
            query,
            album,
            path,
            format,
        } => {
            let lib = Library::open(&index)?;
            let query = Query::parse(&query)?;
            let fmt = match (format, path, album) {
                (Some(f), _, _) => f,
                (None, true, _) => "$path".to_string(),
                (None, false, true) => "$albumartist - $album".to_string(),
                (None, false, false) => "$artist - $album - $title".to_string(),
            };
            let lines: Vec<String> = if album {
                lib.albums(&query)?
                    .iter()
                    .map(|a| library::format(&fmt, |f| a.field(f)))
                    .collect()
            } else {
                lib.items(&query)?
                    .iter()
                    .map(|i| library::format(&fmt, |f| i.field(f)))
                    .collect()
            };
            // A reader that stops early (`| head`) is not an error.
            use std::io::Write;
            let mut out = std::io::stdout().lock();
            for line in lines {
                if writeln!(out, "{line}").is_err() {
                    break;
                }
            }
            Ok(ExitCode::SUCCESS)
        }
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
                    importer.import_as_is(&dir, &sift::Edits::default()).await
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
