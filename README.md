# sift

Matches a folder of music against MusicBrainz, writes its tags, embeds cover
art and files it into a library. A library and a CLI; the CLI reads an
existing beets `config.yaml`, so it can stand in for `beet import`.

```console
$ brew install radiosilence/sift/sift         # or a static binary from the releases
$ sift import ~/Downloads/some-album          # uses ~/.config/beets/config.yaml
$ sift match ~/Downloads/some-album           # show candidates, change nothing
$ sift import --search-id <mbid> <dir>        # apply a specific release
```

## What it reads from a beets config

`directory`, `include`, `import.move`/`import.copy`, `paths.default` and
`paths.comp`, `replace` (in file order), `asciify_paths`, `original_date`,
`per_disc_numbering`, `match.strong_rec_thresh`, whether `fetchart` is among
the `plugins`, and `embedart`/`fetchart` `maxwidth`. Everything else is
ignored rather than rejected.

Path templates are fb2k-style — the template language koan uses, whose engine
this crate carries. A beets template (`$albumartist/%if{$year,($year) }$album`)
is translated on load, so an existing config files albums where beets would
have. `%aunique{}` is dropped: a destination that already exists is refused
instead of disambiguated.

## Decisions

- **Matching** is beets-shaped — a weighted distance over album, artist and
  per-track title and length, with penalties for missing and extra tracks —
  but tracks are paired by disc and track number when the files carry them,
  and by cheapest title-and-length pairing only when they do not. An album is
  applied without asking only when it is below the threshold *and* complete;
  otherwise the candidates are returned.
- **Nothing is overwritten.** Every destination is planned and checked before
  any file changes, including case-insensitive collisions on macOS. Each file
  claims its name with `create_new` before it moves, and a move across
  filesystems goes through a synced, length-checked temporary file.
- **Untrusted tags** cannot escape the library: a slash in a value never
  becomes a directory, empty or relative components are refused rather than
  dropped, and the final path is checked against the library root.
- **Copying leaves the source byte-identical**; tags are written to the copy.
- **MusicBrainz** is called at one request per second with an identifying
  user agent, and a 503 is backed off rather than reported. Cover art comes
  from the Cover Art Archive's own thumbnails, so no image is decoded or
  re-encoded here.

Many of these came from koan, where each was a bug first.
