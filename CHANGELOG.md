# Changelog

## Unreleased

- Files with no disc number that count straight through a release whose sides or discs restart at 1 (A1–A4 and B1–B4 as tracks 1–8) pair with every track, not only the first side's. Such a rip matched a vinyl release with half its tracks "missing" and the other half "extra".
- `sift update` builds a library index (SQLite, in sift's data directory, never beets' `library.db`) from the files' tags, re-reading only files whose size or modification time changed. `sift ls` queries it with beets' syntax: `field:value`, `field::regex`, `field:=exact`, numeric ranges (`year:1990..1999`), `^`/`-` negation, `,` alternatives, `field+`/`field-` sorting, `-a` for albums, `-p` for paths and `-f` for a `$field` format. The index holds nothing the files do not, so deleting it loses nothing.
- A file sift cannot read is remembered by size and modification time and not read again until it changes: a broken file can cost tens of MB each time lofty tries it. `Library::with_workers` limits how many files a scan reads at once, for callers short of memory; scans commit in batches of 2,000.
- `sift move` re-files albums where the current path rules put them, with `--pretend` to list the moves first. Every destination is planned before a file moves; an album whose plan collides with anything is left where it is and the reason printed. An album's cover and other files follow it when it changes directory, and directories it empties are removed.
- `sift duplicates` finds albums held more than once (the same MusicBrainz release, or the same artist, album and track titles) and names the copy to keep: lossless over lossy, then more tracks, then higher resolution, then the one filed under the current rules. `--bin DIR` moves the others there, at their paths relative to the library; nothing is deleted. A copy counts only if the kept one has every track it has, so an album split across folders is not mistaken for a duplicate of itself.
- Replacements that describe the edges of a name (`^\.`, `\.$`, `\s+$`) apply to each path component, as in beets, rather than to every field value inside it. "The Vertigo E.P. [MP3]" had been filed as "The Vertigo E.P- [MP3]", and "R.E.M." inside file names as "R.E.M-".
- An original date of `0000`, which beets writes for an unknown date, no longer files an album under "(0000)"; the release year is used instead.
- ALAC in MP4 is `ALAC` rather than `AAC`, so its folder says `[ALAC]` and it counts as lossless.

- Importing an album that is already filed, every file the same recording in the same place, succeeds and moves nothing. A retry, or a second request queued behind the first, used to fail on the first file already present. A partial overlap is still refused, and nothing is ever overwritten.
- `Importer::check_as_is` answers whether an import as-is would be accepted, and why not, without filing anything.
- `Importer::compare` lines a folder up against one release track by track: pairs with title and length differences, and release tracks with no file and files with no track, for deciding a review.
- `Importer::tracks` returns a folder's files and their tags as an import reads them.
- `import_as_is` takes `Edits`, album-wide and per-file corrections applied before the coherence check, so an edit cannot file what the check would refuse. `Outcome::Imported::release` stays `None` for these.
- `sift import --as-is` (`-A`, as beets' `--noautotag`) and `Importer::import_as_is` file an album by the files' own tags, for releases MusicBrainz does not have. They refuse, saying why, unless the tags describe one album: an album and artist every file agrees on, and a title and distinct track number per file, taken from the file name where the tag is missing. Several artists and no album artist make a Various Artists compilation.
- `Outcome::Imported::release` is optional; it is `None` for an import as-is.
- An album artist of `VA`, `V.A.` or `Various` is searched as "Various Artists", which is how MusicBrainz credits every compilation; the abbreviation found nothing.
- Track lengths may differ by 2% of the track (full penalty at 6%) where that exceeds beets' flat 10 s (30 s). Long sides differ by twenty seconds between a trimmed digital release and a vinyl edit, which the flat allowance read as different recordings; tracks under about eight minutes are judged as before.
- The import log breaks the best match's distance into album, artist, title and length, so a review shows what kept the match from being applied.
- A folder holding an album more than once (FLAC beside WAV, `Track (1).flac` beside `Track.flac`) is imported from one copy of each track, FLAC first; the spares are left in place and logged. Matching every copy counted the spares as extra tracks, and filing them split the album across format-named folders.
- MusicBrainz pacing adapts: refusals widen the gap between requests and successes narrow it again, and when the service's global budget (`X-RateLimit-*`) is nearly spent, requests wait for it to reset. MusicBrainz refuses every client when that budget runs out, so a fixed rate alone cannot avoid it.
- 429, 5xx, timeouts and refused connections are retried, matching beets; previously only 503 was.
- Search hits with as many tracks as the folder are looked up first. Popular albums have a dozen pressings that score alike, and the right one could fall past the lookup limit.
- `ImportError::is_transient` tells a caller whether trying later could succeed, so a service can defer an import rather than fail it.
- First version: MusicBrainz matching, tag writing, Cover Art Archive art, beets config compatibility, fb2k-style path templates, `sift import` and `sift match`.
