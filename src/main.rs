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
        /// Directories, or with `-L` a query over the library.
        paths: Vec<String>,
        /// Re-import albums already in the library that match the query,
        /// retagging them and re-filing any whose tags move them.
        #[arg(short = 'L', long)]
        library: bool,
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
    /// Refresh albums from the MusicBrainz release they were tagged with,
    /// without matching again: for corrections made upstream since.
    Mbsync { query: Vec<String> },
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
    /// Decode matching files and list those whose audio is damaged
    /// (truncated, corrupt packets, FLAC MD5 mismatch). Results are kept
    /// until a file changes, so a re-run checks only what is new.
    #[command(alias = "badfiles")]
    Bad { query: Vec<String> },
    /// Fetch lyrics from LRCLIB for matching tracks that have none: synced
    /// where available, plain otherwise. Tracks already looked up and not
    /// found are not asked for again until they change; -f asks again and
    /// replaces lyrics already present.
    Lyrics {
        query: Vec<String>,
        #[arg(short, long)]
        force: bool,
    },
    /// Measure and write ReplayGain 2.0 track and album gain for matching
    /// albums. Albums already carrying album gain are skipped unless -f.
    Replaygain {
        query: Vec<String>,
        #[arg(short, long)]
        force: bool,
    },
    /// Summarise the library, or the part of it a query matches.
    Stats { query: Vec<String> },
    /// List tracks and discs that matching albums' own totals say are absent.
    Missing { query: Vec<String> },
    /// Move matching albums out of the library into a bin directory, at
    /// their paths relative to it. Nothing is deleted.
    Remove {
        query: Vec<String>,
        #[arg(long)]
        bin: PathBuf,
        /// List the albums, and move nothing.
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

/// `println!`, except that a reader which stops early (`| head`) ends the
/// program quietly rather than panicking it.
macro_rules! say {
    ($($arg:tt)*) => {{
        use std::io::Write;
        if writeln!(std::io::stdout().lock(), $($arg)*).is_err() {
            std::process::exit(0);
        }
    }};
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
            say!(
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
                        say!("move  {}  →  {}", album.dir.display(), to.display());
                        if verbose {
                            for m in &moves {
                                say!("        {}  →  {}", m.from.display(), m.to.display());
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
            say!("{moved} albums {verb}, {refused} left where they are");
            Ok(ExitCode::SUCCESS)
        }
        Command::Bad { query } => {
            let mut lib = Library::open(&index)?;
            lib.update(&cfg.directory)?;
            let items = lib.items(&Query::parse(&query)?)?;
            let verdicts = lib.check(&items)?;
            let (mut bad, mut unchecked) = (0, 0);
            for (path, v) in &verdicts {
                match v {
                    sift::check::Verdict::Bad(why) => {
                        bad += 1;
                        say!("{}: {why}", path.display());
                    }
                    sift::check::Verdict::Unchecked(_) => unchecked += 1,
                    sift::check::Verdict::Ok => {}
                }
            }
            say!(
                "{} files checked: {bad} damaged, {unchecked} in formats that cannot be checked here",
                verdicts.len()
            );
            Ok(if bad > 0 {
                ExitCode::from(1)
            } else {
                ExitCode::SUCCESS
            })
        }
        Command::Lyrics { query, force } => {
            use sift::lyrics::Lyrics;
            let mut lib = Library::open(&index)?;
            lib.update(&cfg.directory)?;
            let misses = if force {
                Default::default()
            } else {
                lib.lyrics_misses()?
            };
            let client = sift::lyrics::Client::new();
            let (mut synced, mut plain, mut none) = (0, 0, 0);
            for item in lib.items(&Query::parse(&query)?)? {
                let t = &item.track;
                let key = t.path.to_string_lossy().into_owned();
                if misses.get(&key) == Some(&(item.size, item.mtime))
                    || (!force && sift::meta::has_lyrics(&t.path))
                {
                    continue;
                }
                let (Some(artist), Some(title)) = (&t.artist, &t.title) else {
                    continue;
                };
                let found = client
                    .get(
                        artist,
                        title,
                        t.album.as_deref().unwrap_or(""),
                        t.duration.as_secs(),
                    )
                    .await;
                match found {
                    Ok(Lyrics::Synced(l)) => {
                        sift::meta::set_lyrics(&t.path, &l)?;
                        synced += 1;
                    }
                    Ok(Lyrics::Plain(l)) => {
                        sift::meta::set_lyrics(&t.path, &l)?;
                        plain += 1;
                    }
                    Ok(Lyrics::Instrumental | Lyrics::NotFound) => {
                        lib.record_lyrics_miss(&item)?;
                        none += 1;
                    }
                    Err(e) => eprintln!("{}: {e}", t.path.display()),
                }
            }
            lib.update(&cfg.directory)?;
            say!("{synced} synced, {plain} plain, {none} without lyrics");
            Ok(ExitCode::SUCCESS)
        }
        Command::Replaygain { query, force } => {
            let mut lib = Library::open(&index)?;
            lib.update(&cfg.directory)?;
            let workers = std::thread::available_parallelism().map_or(4, |n| n.get().min(4));
            let (mut done, mut skipped, mut failed) = (0, 0, 0);
            for album in lib.albums(&Query::parse(&query)?)? {
                let paths: Vec<PathBuf> =
                    album.items.iter().map(|i| i.track.path.clone()).collect();
                if !force && paths.iter().all(|p| sift::meta::has_album_gain(p)) {
                    skipped += 1;
                    continue;
                }
                match sift::replaygain::album(&paths, workers) {
                    Ok((tracks, album_gain)) => {
                        for (p, g) in paths.iter().zip(tracks) {
                            sift::meta::set_replaygain(p, g, album_gain)?;
                        }
                        done += 1;
                        say!("{:+.2} dB  {}", album_gain.db, album.dir.display());
                    }
                    Err(e) => {
                        failed += 1;
                        eprintln!("skipped  {}: {e}", album.dir.display());
                    }
                }
            }
            lib.update(&cfg.directory)?;
            say!(
                "{done} albums measured, {skipped} already had gain, {failed} could not be measured"
            );
            Ok(ExitCode::SUCCESS)
        }
        Command::Stats { query } => {
            let mut lib = Library::open(&index)?;
            lib.update(&cfg.directory)?;
            let query = Query::parse(&query)?;
            let items = lib.items(&query)?;
            let albums = lib.albums(&query)?;
            let bytes: u64 = items.iter().map(|i| i.size).sum();
            let secs: f64 = items.iter().map(|i| i.track.duration.as_secs_f64()).sum();
            let mut formats: std::collections::BTreeMap<&str, usize> = Default::default();
            for i in &items {
                *formats.entry(i.track.format.as_str()).or_default() += 1;
            }
            let artists: std::collections::HashSet<_> = albums
                .iter()
                .filter_map(|a| a.items.first()?.field("albumartist"))
                .map(|v| format!("{v:?}"))
                .collect();
            say!("Tracks:  {}", items.len());
            say!("Albums:  {}", albums.len());
            say!("Artists: {}", artists.len());
            say!("Size:    {:.1} GB", bytes as f64 / 1e9);
            say!("Time:    {:.1} days", secs / 86_400.0);
            let formats: Vec<String> = formats.iter().map(|(f, n)| format!("{f} {n}")).collect();
            say!("Formats: {}", formats.join(", "));
            Ok(ExitCode::SUCCESS)
        }
        Command::Missing { query } => {
            let mut lib = Library::open(&index)?;
            lib.update(&cfg.directory)?;
            let mut count = 0;
            for album in lib.albums(&Query::parse(&query)?)? {
                let gaps = manage::missing(&album);
                if !gaps.is_empty() {
                    count += 1;
                    say!("{}: {}", album.dir.display(), gaps.join(", "));
                }
            }
            say!("{count} albums with gaps");
            Ok(ExitCode::SUCCESS)
        }
        Command::Remove {
            query,
            bin,
            pretend,
        } => {
            anyhow::ensure!(
                !query.is_empty(),
                "give a query; removing the whole library takes an explicit \"\""
            );
            let mut lib = Library::open(&index)?;
            lib.update(&cfg.directory)?;
            for album in lib.albums(&Query::parse(&query)?)? {
                if pretend {
                    say!("would bin  {}", album.dir.display());
                } else {
                    let to = manage::bin(&mut lib, &cfg.directory, &bin, &album).await?;
                    say!("binned  {}  →  {}", album.dir.display(), to.display());
                }
            }
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
                say!("{verb}  {}", f.display());
            }
            for (from, to) in &r.moved {
                say!("moved  {}  →  {}", from.display(), to.display());
            }
            for (dir, why) in &r.left {
                eprintln!("left   {}: {why}", dir.display());
            }
            say!(
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
                say!("keep  {}", d.keep.dir.display());
                for (other, why) in &d.others {
                    match &bin {
                        Some(bin) => {
                            let dest = manage::bin(&mut lib, &cfg.directory, bin, other).await?;
                            say!(
                                "  bin {}  ({why})  →  {}",
                                other.dir.display(),
                                dest.display()
                            );
                        }
                        None => say!("  dup {}  ({why})", other.dir.display()),
                    }
                }
            }
            say!("{} albums held more than once", dupes.len());
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
            library,
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
            let jobs: Vec<(PathBuf, Option<String>)> = if library {
                anyhow::ensure!(
                    !as_is,
                    "-L re-imports against MusicBrainz; it cannot be combined with --as-is"
                );
                let mut lib = Library::open(&index)?;
                lib.update(&cfg.directory)?;
                lib.albums(&Query::parse(&paths)?)?
                    .into_iter()
                    .map(|a| (a.dir, search_id.clone()))
                    .collect()
            } else {
                paths
                    .iter()
                    .map(|p| (PathBuf::from(p), search_id.clone()))
                    .collect()
            };
            imports(Importer::new(cfg), jobs, library, as_is, log.as_deref()).await
        }
        Command::Mbsync { query } => {
            let mut lib = Library::open(&index)?;
            lib.update(&cfg.directory)?;
            let jobs = lib
                .albums(&Query::parse(&query)?)?
                .into_iter()
                .filter_map(|a| {
                    let id = a.items.first()?.track.mb_album_id.clone()?;
                    Some((a.dir, Some(id)))
                })
                .collect();
            imports(Importer::new(cfg), jobs, true, false, None).await
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
                        say!(
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

/// Import (or, `in_place`, re-import) each directory, with its release when
/// one is given, printing a line per album and appending one to `log`.
async fn imports(
    importer: Importer,
    jobs: Vec<(PathBuf, Option<String>)>,
    in_place: bool,
    as_is: bool,
    log: Option<&std::path::Path>,
) -> anyhow::Result<ExitCode> {
    let mut failed = false;
    for (dir, release) in jobs {
        let outcome = if as_is {
            importer.import_as_is(&dir, &sift::Edits::default()).await
        } else if in_place {
            importer.reimport(&dir, release.as_deref()).await
        } else {
            importer.import(&dir, release.as_deref()).await
        };
        let line = match outcome {
            Ok(Outcome::Imported {
                dir: dest,
                release: Some(release),
                ..
            }) => {
                say!(
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
                say!("imported  as-is  →  {}", dest.display());
                format!("import as-is {}", dir.display())
            }
            Ok(Outcome::Review {
                reason, candidates, ..
            }) => {
                failed = true;
                say!("skipped   {}: {reason}", dir.display());
                for c in candidates.iter().take(5) {
                    say!(
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
