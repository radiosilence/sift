//! Configuration, read from a beets `config.yaml` so an existing setup keeps
//! working unchanged.
//!
//! Only what sift acts on is read; everything else in the file is ignored
//! rather than rejected, so a config full of plugin settings still loads.
//! Path templates may be beets' `$field`/`%func{}` syntax or sift's own
//! fb2k-style syntax; beets templates are translated on load.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("{path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path}: {source}")]
    Parse {
        path: PathBuf,
        source: serde_yaml_ng::Error,
    },
    #[error("replace pattern {pattern:?}: {source}")]
    Regex {
        pattern: String,
        source: regex::Error,
    },
}

#[derive(Debug, Clone)]
pub struct Config {
    pub directory: PathBuf,
    /// fb2k-style templates, without extension.
    pub path_default: String,
    pub path_comp: String,
    pub replace: Vec<(regex::Regex, String)>,
    pub asciify_paths: bool,
    /// Take the year from the first release of the release group, so a
    /// 2011 remaster of a 1977 album files under 1977.
    pub original_date: bool,
    /// Number tracks within each disc rather than across the release.
    pub per_disc_numbering: bool,
    pub move_files: bool,
    pub fetch_art: bool,
    /// Largest cover width to embed; Cover Art Archive serves 250, 500 and
    /// 1200 thumbnails, and the nearest one at or below this is used.
    pub art_max_width: u32,
    /// Smallest cover width accepted; a smaller candidate is skipped.
    pub art_min_width: u32,
    /// JPEG quality used when a cover is resized and re-encoded.
    pub art_quality: u8,
    /// How far from square a cover may be before it is rejected.
    pub art_ratio: Option<Ratio>,
    /// Prefer the Cover Art Archive's full-size original over its
    /// thumbnails.
    pub art_high_resolution: bool,
    /// Below this distance a match is applied without asking.
    pub strong_threshold: f64,
    pub musicbrainz_contact: String,
    /// Where MusicBrainz responses are kept between imports.
    pub cache_dir: Option<PathBuf>,
    /// Keep a file's own modification time as its library `added` time, and
    /// preserve mtimes across moves, copies and tag writes, so a re-import
    /// or re-file never makes an old album look newly added. True when
    /// beets' `importadded` plugin is enabled. beets also has
    /// `importadded.preserve_mtimes` and `importadded.preserve_write_mtimes`
    /// options, both true by default; sift does not read them and always
    /// behaves as if both are on once the plugin is listed.
    pub import_added: bool,
    /// beets' `ftintitle` plugin: fold a featured artist out of the track
    /// artist and into the title. `None` when the plugin isn't enabled.
    pub ft_in_title: Option<FtInTitle>,
    /// Set only when the `discogs` plugin is listed and a token is
    /// available, from `discogs.user_token` or `DISCOGS_TOKEN`.
    pub discogs: Option<DiscogsConf>,
}

#[derive(Debug, Clone)]
pub struct FtInTitle {
    /// Drop the featured artist instead of adding it to the title.
    pub drop: bool,
    /// Where `{0}` is the featured artist, e.g. `"feat. {0}"`.
    pub format: String,
}

#[derive(Debug, Clone)]
pub struct DiscogsConf {
    pub token: String,
    /// Prefix a medley's sub-tracks with the enclosing index track's title.
    pub index_tracks: bool,
}

/// The template beets ships with, translated.
pub const DEFAULT_PATH: &str = "%album artist%/%album%/$num(%tracknumber%,2) %title%";

impl Default for Config {
    fn default() -> Self {
        Self {
            directory: PathBuf::new(),
            path_default: DEFAULT_PATH.into(),
            path_comp: "Compilations/%album%/$num(%tracknumber%,2) %title%".into(),
            replace: beets_default_replace(),
            asciify_paths: false,
            original_date: false,
            per_disc_numbering: false,
            move_files: false,
            fetch_art: true,
            art_max_width: 1200,
            art_min_width: 0,
            art_quality: 90,
            art_ratio: None,
            art_high_resolution: false,
            strong_threshold: 0.04,
            musicbrainz_contact: "https://github.com/radiosilence/sift".into(),
            cache_dir: dirs::cache_dir().map(|d| d.join("sift")),
            import_added: false,
            ft_in_title: None,
            discogs: None,
        }
    }
}

fn beets_default_replace() -> Vec<(regex::Regex, String)> {
    [
        (r"[\\/]", "_"),
        (r"^\.", "_"),
        (r"[\x00-\x1f]", "_"),
        (r#"[<>:"\?\*\|]"#, "_"),
        (r"\.$", "_"),
        (r"\s+$", ""),
        (r"^\s+", ""),
        (r"^-", "_"),
    ]
    .into_iter()
    .map(|(p, r)| (regex::Regex::new(p).expect("static pattern"), r.to_string()))
    .collect()
}

/// How far from square a cover may be, as beets' fetchart `enforce_ratio`
/// reads it: a percentage of the longer side, or a fixed number of pixels.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Ratio {
    Percent(f32),
    Pixels(u32),
}

impl Ratio {
    /// Parses "10%", "10px" or a bare "10" (pixels), as beets does.
    fn parse(s: &str) -> Option<Self> {
        let s = s.trim();
        if let Some(pct) = s.strip_suffix('%') {
            return pct.trim().parse().ok().map(Ratio::Percent);
        }
        s.strip_suffix("px")
            .unwrap_or(s)
            .trim()
            .parse()
            .ok()
            .map(Ratio::Pixels)
    }

    /// Whether `width`x`height` is close enough to square.
    pub fn allows(self, width: u32, height: u32) -> bool {
        let tolerance = match self {
            Ratio::Percent(p) => p / 100.0 * width.max(height) as f32,
            Ratio::Pixels(px) => px as f32,
        };
        (width as i64 - height as i64).unsigned_abs() as f32 <= tolerance
    }
}

#[derive(Debug, Default, Deserialize)]
struct Raw {
    #[serde(default)]
    include: Vec<String>,
    directory: Option<String>,
    #[serde(default)]
    import: RawImport,
    #[serde(default)]
    paths: BTreeMap<String, String>,
    /// Ordered: beets applies these in file order, and so must we.
    replace: Option<serde_yaml_ng::Mapping>,
    asciify_paths: Option<bool>,
    original_date: Option<bool>,
    per_disc_numbering: Option<bool>,
    #[serde(default)]
    plugins: PluginList,
    #[serde(default)]
    fetchart: RawArt,
    #[serde(default)]
    embedart: RawArt,
    #[serde(rename = "match", default)]
    matching: RawMatch,
    #[serde(default)]
    discogs: RawDiscogs,
    ftintitle: Option<RawFtInTitle>,
}

#[derive(Debug, Default, Deserialize)]
struct RawFtInTitle {
    auto: Option<bool>,
    drop: Option<bool>,
    format: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawImport {
    #[serde(rename = "move")]
    move_files: Option<bool>,
    copy: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawArt {
    maxwidth: Option<u32>,
    minwidth: Option<u32>,
    quality: Option<u8>,
    enforce_ratio: Option<String>,
    high_resolution: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct RawMatch {
    strong_rec_thresh: Option<f64>,
}

#[derive(Debug, Default, Deserialize)]
struct RawDiscogs {
    user_token: Option<String>,
    index_tracks: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(untagged)]
enum PluginList {
    #[default]
    None,
    List(Vec<String>),
    Line(String),
}

impl PluginList {
    fn contains(&self, name: &str) -> bool {
        match self {
            Self::None => false,
            Self::List(l) => l.iter().any(|p| p == name),
            Self::Line(s) => s.split_whitespace().any(|p| p == name),
        }
    }
}

impl Config {
    /// Load a beets config, following `include:` relative to its directory.
    /// Later files override earlier ones, as in beets: includes first, then
    /// the file itself.
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let mut layers = Vec::new();
        collect(path, &mut layers, 0)?;
        let mut cfg = Self::default();
        let mut art_width = None;
        let mut art_min_width = None;
        let mut art_quality = None;
        let mut art_ratio = None;
        let mut art_high_resolution = None;
        let mut fetchart = false;
        let mut ftintitle_enabled = false;
        let mut ft_auto = None;
        let mut ft_drop = None;
        let mut ft_format = None;
        let mut discogs_enabled = false;
        let mut discogs_token = None;
        let mut discogs_index_tracks = false;
        for raw in layers {
            if let Some(d) = raw.directory {
                cfg.directory = expand(&d);
            }
            if let Some(m) = raw.import.move_files {
                cfg.move_files = m;
            }
            if raw.import.copy == Some(false) && raw.import.move_files.is_none() {
                cfg.move_files = true;
            }
            if let Some(t) = raw.paths.get("default") {
                cfg.path_default = translate(t);
            }
            if let Some(t) = raw.paths.get("comp") {
                cfg.path_comp = translate(t);
            }
            if let Some(map) = raw.replace {
                cfg.replace = map
                    .into_iter()
                    .filter_map(|(k, v)| {
                        Some((
                            k.as_str()?.to_string(),
                            v.as_str().unwrap_or_default().to_string(),
                        ))
                    })
                    .map(|(p, r)| {
                        regex::Regex::new(&p)
                            .map(|re| (re, r))
                            .map_err(|source| ConfigError::Regex { pattern: p, source })
                    })
                    .collect::<Result<_, _>>()?;
            }
            cfg.asciify_paths = raw.asciify_paths.unwrap_or(cfg.asciify_paths);
            cfg.original_date = raw.original_date.unwrap_or(cfg.original_date);
            cfg.per_disc_numbering = raw.per_disc_numbering.unwrap_or(cfg.per_disc_numbering);
            cfg.import_added |= raw.plugins.contains("importadded");
            fetchart |= raw.plugins.contains("fetchart");
            art_width = raw
                .embedart
                .maxwidth
                .or(raw.fetchart.maxwidth)
                .or(art_width);
            art_min_width = raw
                .embedart
                .minwidth
                .or(raw.fetchart.minwidth)
                .or(art_min_width);
            art_quality = raw
                .embedart
                .quality
                .or(raw.fetchart.quality)
                .or(art_quality);
            art_high_resolution = raw
                .embedart
                .high_resolution
                .or(raw.fetchart.high_resolution)
                .or(art_high_resolution);
            art_ratio = raw
                .embedart
                .enforce_ratio
                .as_deref()
                .or(raw.fetchart.enforce_ratio.as_deref())
                .and_then(Ratio::parse)
                .or(art_ratio);
            if let Some(t) = raw.matching.strong_rec_thresh {
                cfg.strong_threshold = t;
            }
            ftintitle_enabled |= raw.plugins.contains("ftintitle");
            if let Some(ft) = raw.ftintitle {
                ft_auto = ft.auto.or(ft_auto);
                ft_drop = ft.drop.or(ft_drop);
                ft_format = ft.format.or(ft_format);
            }
            discogs_enabled |= raw.plugins.contains("discogs");
            discogs_token = raw.discogs.user_token.or(discogs_token);
            discogs_index_tracks = raw.discogs.index_tracks.unwrap_or(discogs_index_tracks);
        }
        cfg.fetch_art = fetchart;
        if let Some(w) = art_width {
            cfg.art_max_width = w;
        }
        if ftintitle_enabled && ft_auto != Some(false) {
            cfg.ft_in_title = Some(FtInTitle {
                drop: ft_drop.unwrap_or(false),
                format: ft_format.unwrap_or_else(|| "feat. {0}".into()),
            });
        }
        if let Some(w) = art_min_width {
            cfg.art_min_width = w;
        }
        if let Some(q) = art_quality {
            cfg.art_quality = q;
        }
        if let Some(r) = art_ratio {
            cfg.art_ratio = Some(r);
        }
        if let Some(h) = art_high_resolution {
            cfg.art_high_resolution = h;
        }
        let token = std::env::var("DISCOGS_TOKEN").ok().or(discogs_token);
        cfg.discogs = discogs_enabled
            .then_some(token)
            .flatten()
            .map(|token| DiscogsConf {
                token,
                index_tracks: discogs_index_tracks,
            });
        // No `directory` is allowed: a shared base config often leaves it to
        // a per-machine file, and a caller may set it after loading. Whoever
        // imports checks it is set.
        Ok(cfg)
    }

    /// `$BEETSDIR/config.yaml`, then `~/.config/beets/config.yaml`.
    pub fn default_path() -> Option<PathBuf> {
        std::env::var_os("BEETSDIR")
            .map(|d| PathBuf::from(d).join("config.yaml"))
            .or_else(|| dirs::home_dir().map(|h| h.join(".config/beets/config.yaml")))
            .filter(|p| p.exists())
    }
}

fn collect(path: &Path, out: &mut Vec<Raw>, depth: usize) -> Result<(), ConfigError> {
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.into(),
        source,
    })?;
    let raw: Raw = if text.trim().is_empty() {
        Raw::default()
    } else {
        serde_yaml_ng::from_str(&text).map_err(|source| ConfigError::Parse {
            path: path.into(),
            source,
        })?
    };
    if depth < 8 {
        let dir = path.parent().unwrap_or(Path::new("."));
        for inc in &raw.include {
            collect(&dir.join(expand(inc)), out, depth + 1)?;
        }
    }
    out.push(raw);
    Ok(())
}

fn expand(p: &str) -> PathBuf {
    match p.strip_prefix("~/") {
        Some(rest) => dirs::home_dir()
            .map(|h| h.join(rest))
            .unwrap_or_else(|| PathBuf::from(p)),
        None => PathBuf::from(p),
    }
}

/// beets field names to sift's.
fn field(name: &str) -> String {
    match name {
        "albumartist" => "%album artist%".into(),
        "track" => "$num(%tracknumber%,2)".into(),
        "disc" => "$num(%discnumber%,2)".into(),
        "tracktotal" => "%totaltracks%".into(),
        "disctotal" => "%totaldiscs%".into(),
        "format" => "%codec%".into(),
        "original_year" => "%original year%".into(),
        "albumtype" => "%album type%".into(),
        "catalognum" => "%catalog number%".into(),
        "mb_albumid" => "%musicbrainz album id%".into(),
        other => format!("%{other}%"),
    }
}

/// Translate a beets path template into fb2k syntax. A template that uses
/// no beets syntax is returned as it is, so sift's own templates pass
/// through.
pub fn translate(template: &str) -> String {
    if !template.contains('$') && !template.contains("%if{") && !template.contains('{') {
        return template.to_string();
    }
    if template.contains("$num(") || template.contains("$if(") {
        return template.to_string();
    }
    let chars: Vec<char> = template.chars().collect();
    let (out, _) = translate_until(&chars, 0, &[]);
    out
}

/// Translate from `i` until one of `stops` at nesting depth zero.
fn translate_until(c: &[char], mut i: usize, stops: &[char]) -> (String, usize) {
    let mut out = String::new();
    let mut literal = String::new();
    let flush = |literal: &mut String, out: &mut String| {
        if literal.is_empty() {
            return;
        }
        if literal.chars().any(|ch| "[]'%$(),".contains(ch)) {
            out.push('\'');
            out.push_str(&literal.replace('\'', "''"));
            out.push('\'');
        } else {
            out.push_str(literal);
        }
        literal.clear();
    };
    while i < c.len() {
        let ch = c[i];
        if stops.contains(&ch) {
            break;
        }
        if ch == '$' && c.get(i + 1) == Some(&'$') {
            literal.push('$');
            i += 2;
        } else if ch == '$' && c.get(i + 1) == Some(&'{') {
            let end = c[i..]
                .iter()
                .position(|&x| x == '}')
                .map_or(c.len(), |p| i + p);
            flush(&mut literal, &mut out);
            out.push_str(&field(&c[i + 2..end].iter().collect::<String>()));
            i = end + 1;
        } else if ch == '$'
            && c.get(i + 1)
                .is_some_and(|x| x.is_alphanumeric() || *x == '_')
        {
            let start = i + 1;
            let mut end = start;
            while end < c.len() && (c[end].is_alphanumeric() || c[end] == '_') {
                end += 1;
            }
            flush(&mut literal, &mut out);
            out.push_str(&field(&c[start..end].iter().collect::<String>()));
            i = end;
        } else if ch == '%'
            && c[i + 1..].iter().position(|&x| x == '{').is_some_and(|p| {
                c[i + 1..i + 1 + p]
                    .iter()
                    .all(|x| x.is_alphanumeric() || *x == '_')
            })
        {
            let name_end = i + 1 + c[i + 1..].iter().position(|&x| x == '{').unwrap();
            let name: String = c[i + 1..name_end].iter().collect();
            let mut args = Vec::new();
            let mut j = name_end + 1;
            loop {
                let (arg, next) = translate_until(c, j, &[',', '}']);
                args.push(arg);
                j = next;
                match c.get(j) {
                    Some(',') => j += 1,
                    _ => break,
                }
            }
            flush(&mut literal, &mut out);
            out.push_str(&function(&name, &args));
            i = j + 1;
        } else {
            literal.push(ch);
            i += 1;
        }
    }
    flush(&mut literal, &mut out);
    (out, i)
}

fn function(name: &str, args: &[String]) -> String {
    let arg = |n: usize| args.get(n).cloned().unwrap_or_default();
    match name {
        // Disambiguates albums with identical paths; sift refuses the
        // collision instead, which is visible rather than silent.
        "aunique" => String::new(),
        "if" => format!("$if({},{},{})", arg(0), arg(1), arg(2)),
        "left" => format!("$left({},{})", arg(0), arg(1)),
        "right" => format!("$right({},{})", arg(0), arg(1)),
        "upper" => format!("$upper({})", arg(0)),
        "lower" => format!("$lower({})", arg(0)),
        "title" => format!("$caps({})", arg(0)),
        "ifdef" => format!(
            "$if({},{},{})",
            field(args.first().map_or("", |s| s.trim_matches('%'))),
            arg(1),
            arg(2)
        ),
        other => format!("${other}({})", args.join(",")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translates_the_users_beets_template() {
        let t = translate(
            "$albumartist/%if{$year,($year) }$album%aunique{} [$format]/$disc$track. $artist - $title",
        );
        let render = |year: Option<&str>| {
            let year = year.map(str::to_string);
            let fields = move |name: &str| match name {
                "album artist" | "artist" => Some("Daisy the Great".to_string()),
                "year" => year.clone(),
                "album" => Some("All You Need Is Time".into()),
                "codec" => Some("FLAC".into()),
                "discnumber" => Some("1".into()),
                "tracknumber" => Some("2".into()),
                "title" => Some("Glitter".into()),
                _ => None,
            };
            crate::format::format(&t, &fields).unwrap()
        };
        assert_eq!(
            render(Some("2022")),
            "Daisy the Great/(2022) All You Need Is Time [FLAC]/0102. Daisy the Great - Glitter"
        );
        assert_eq!(
            render(None),
            "Daisy the Great/All You Need Is Time [FLAC]/0102. Daisy the Great - Glitter"
        );
    }

    #[test]
    fn fb2k_templates_pass_through() {
        let t = "%album artist%/['('%year%') ']%album%";
        assert_eq!(translate(t), t);
    }

    #[test]
    fn loads_includes_in_order_and_ignores_unknown_keys() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("base.yaml"),
            "original_date: true\nper_disc_numbering: true\nimport:\n  move: true\npaths:\n  default: $albumartist/$album/$track $title\nreplace:\n  '[\\\\/]': '-'\n  '[<>:\"\\?\\*\\|]': '-'\nplugins:\n  - fetchart\n  - embedart\nembedart:\n  maxwidth: 1200\nfetchart:\n  minwidth: 500\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            "include: [./base.yaml]\ndirectory: /music\nlastgenre:\n  whatever: 1\n",
        )
        .unwrap();
        let cfg = Config::load(&dir.path().join("config.yaml")).unwrap();
        assert_eq!(cfg.directory, PathBuf::from("/music"));
        assert!(cfg.original_date && cfg.per_disc_numbering && cfg.move_files && cfg.fetch_art);
        assert_eq!(cfg.art_max_width, 1200);
        assert_eq!(cfg.art_min_width, 500);
        assert_eq!(
            cfg.path_default,
            "%album artist%/%album%/$num(%tracknumber%,2) %title%"
        );
        assert_eq!(cfg.replace.len(), 2);
        assert_eq!(
            cfg.replace[1]
                .0
                .replace_all("a:b", cfg.replace[1].1.as_str()),
            "a-b"
        );
    }

    #[test]
    fn ftintitle_plugin_enables_it_with_options() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            "directory: /music\nplugins: [ftintitle]\nftintitle:\n  drop: true\n",
        )
        .unwrap();
        let cfg = Config::load(&dir.path().join("config.yaml")).unwrap();
        let ft = cfg.ft_in_title.expect("ftintitle should be enabled");
        assert!(ft.drop);
        assert_eq!(ft.format, "feat. {0}");
    }

    #[test]
    fn loads_fetchart_quality_and_ratio() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            "directory: /music\nfetchart:\n  quality: 95\n  enforce_ratio: 10%\n  high_resolution: true\n",
        )
        .unwrap();
        let cfg = Config::load(&dir.path().join("config.yaml")).unwrap();
        assert_eq!(cfg.art_quality, 95);
        assert_eq!(cfg.art_ratio, Some(Ratio::Percent(10.0)));
        assert!(cfg.art_high_resolution);
    }

    #[test]
    fn ratio_tolerance() {
        assert_eq!(Ratio::parse("10%"), Some(Ratio::Percent(10.0)));
        assert_eq!(Ratio::parse("10px"), Some(Ratio::Pixels(10)));
        assert_eq!(Ratio::parse("10"), Some(Ratio::Pixels(10)));

        assert!(Ratio::Percent(10.0).allows(1000, 950));
        assert!(!Ratio::Percent(10.0).allows(1000, 800));
        assert!(!Ratio::Pixels(10).allows(1000, 980));
    }
}
