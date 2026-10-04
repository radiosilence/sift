# sift

Matches a folder of music against MusicBrainz, writes its tags, embeds cover
art and files it into a library. A library and a CLI; the CLI reads an
existing beets `config.yaml`, so it can stand in for `beet import`.

The library is published to crates.io as `sift-music`, since `sift` was taken;
it is still imported as `sift`:

```toml
sift = { package = "sift-music", version = "0.4" }
```

```console
$ mise use -g github:radiosilence/sift        # or brew install radiosilence/sift/sift,
                                              # or a static binary from the releases
$ sift import ~/Downloads/some-album          # uses ~/.config/beets/config.yaml
$ sift match ~/Downloads/some-album           # show candidates, change nothing
$ sift import --search-id <mbid> <dir>        # apply a specific release
$ sift update                                 # index the library
$ sift ls -a year:1990..1999 format:FLAC      # beets query syntax
$ sift move --pretend                         # what re-filing would change
$ sift duplicates --bin ~/music-bin           # spare copies out of the library
$ sift modify -a album:untru album=Untrue      # fix tags, re-file what moves
$ sift import -L artist:burial                # match library albums again
$ sift mbsync year:2020..                     # refresh from MusicBrainz
$ sift missing ; sift bad ; sift stats         # gaps, damaged audio, totals
```

## What it reads from a beets config

`directory`, `include`, `import.move`/`import.copy`, `paths.default` and
`paths.comp`, `replace` (in file order), `asciify_paths`, `original_date`,
`per_disc_numbering`, `match.strong_rec_thresh`, whether `fetchart` is among
the `plugins`, and `embedart`/`fetchart` `maxwidth`. Everything else is
ignored rather than rejected.

Discogs is consulted as a second source, as beets' `discogs` plugin does,
when MusicBrainz has no match under `match.strong_rec_thresh` and the
`plugins` list includes `discogs`. It needs a personal access token, from
`discogs.user_token` or the `DISCOGS_TOKEN` environment variable, generated
at <https://www.discogs.com/settings/developers>; `discogs.index_tracks`
(default off) prefixes a medley's sub-tracks with the enclosing index
track's title, as beets' option of the same name does.

Path templates are fb2k-style — the template language koan uses, whose engine
this crate carries. A beets template (`$albumartist/%if{$year,($year) }$album`)
is translated on load, so an existing config files albums where beets would
have. `%aunique{}` is dropped: a destination that already exists is refused
instead of disambiguated.

## Decisions

- **Matching** is beets-shaped — a weighted distance over album, artist and
  per-track title and length, with penalties for missing and extra tracks —
  but tracks are paired by disc and track number when the files carry them,
  and by cheapest title-and-length pairing only when they do not. The length
  allowance is beets' 10 s (full penalty at 30 s) or 2% (6%) of the track,
  whichever is larger, since a long side routinely differs by more than ten
  seconds between editions. An album is applied without asking only when it
  is below the threshold *and* complete; otherwise the candidates are
  returned, with what the distance is made of in the log.
- **As-is imports are gated.** Filing by the files' own tags (`--as-is`)
  requires one album and one artist across every file and a title and
  distinct track number for each; otherwise nothing moves and the reason is
  given, since a library filed from bad tags is worse than an album waiting.
- **One copy of each track.** A folder holding an album in two formats, or
  with `(1)` duplicates, is imported from the best copy of each track.
- **Nothing is overwritten.** Every destination is planned and checked before
  any file changes, including case-insensitive collisions on macOS. Each file
  claims its name with `create_new` before it moves, and a move across
  filesystems goes through a synced, length-checked temporary file.
- **Untrusted tags** cannot escape the library: a slash in a value never
  becomes a directory, empty or relative components are refused rather than
  dropped, and the final path is checked against the library root.
- **Copying leaves the source byte-identical**; tags are written to the copy.
- **MusicBrainz** is called at most once a second with an identifying user
  agent. The service also has a global budget shared by every client, and
  refuses everyone when it runs out, so pacing adapts: refusals slow requests
  down, successes speed them back up, and a nearly spent budget is waited out.
  Refusals, server errors and dropped connections are retried; responses are
  cached on disk so a retried import asks for nothing twice. Cover art comes
  from the Cover Art Archive's own thumbnails, so no image is decoded or
  re-encoded here.

- **The library index is derived, never authoritative.** `sift update`
  reads the files into SQLite so queries are milliseconds instead of a read
  of every file; it holds nothing the files do not, and beets' own
  `library.db` is never read or written, since its schema is beets'
  internals. Queries are evaluated in Rust rather than translated to SQL, so
  their semantics are beets' exactly.
- **Changing the library is album-at-a-time and planned first.** `move` and
  `duplicates --bin` check every destination before any file moves and leave
  an album alone rather than move part of it. Duplicates are moved to a bin,
  not deleted.

Many of these came from koan, where each was a bug first.
