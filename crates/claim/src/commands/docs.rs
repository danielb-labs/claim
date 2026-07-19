//! `claim docs`: open the product documentation that ships inside the binary.
//!
//! An installed `claim` is a single static binary with no repository behind it, so
//! the docs a user reaches for cannot live only in `docs/` on disk — they must
//! travel *in* the binary and be the docs for *that* binary. This verb embeds the
//! whole documentation site at compile time with [`include_str!`]/[`include_bytes!`]
//! (see [`BUNDLE`]), so the version a user opens is version-locked to the tool they
//! ran: an old binary can never show a newer site than it was built from, and a new
//! binary never shows a stale one. There is no network fetch and no "latest docs"
//! that could drift from the installed behavior.
//!
//! At runtime the bundle is materialized into a per-version cache directory (so the
//! relative `assets/` image links and `.md` cross-links in the HTML resolve as a
//! normal local site) and its path is printed — the headless-first default, so
//! `open "$(claim docs)"` composes. `--open` additionally hands the page to the
//! platform's file opener; a missing opener (a CI box, a bare server) degrades to
//! printing the path and exiting `0` rather than failing — a doc you can't auto-open
//! is still a doc you can `open` yourself.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::DocsArgs;
use crate::output::{emit, note, Format};

/// One bundled documentation file: its site-relative path and its bytes, captured
/// from the repository's `docs/` tree at compile time.
///
/// The `rel` path is exactly the file's location under `docs/` (e.g. `index.html`,
/// `assets/lifecycle.png`), so writing every entry at `rel` under the cache
/// directory reproduces the site's own layout and every relative link in the HTML
/// resolves unchanged.
struct BundledFile {
    /// The path relative to the site root, using `/` separators.
    rel: &'static str,
    /// The file's verbatim bytes.
    bytes: &'static [u8],
}

/// The embedded documentation site.
///
/// Every file the site references is here: the `index.html` overview, the three
/// diagram PNGs its `<img>` tags point at, and the topic Markdown pages its links
/// reach. Paths are relative to the `claim` crate manifest (`crates/claim`), so the
/// workspace-root `docs/` tree is two levels up. If a new page or asset is added to
/// the site, it must be added here too, or `claim docs` would open a site with a
/// broken link — a gap the docs-coverage claim in this repo's own store is meant to
/// keep visible.
const BUNDLE: &[BundledFile] = &[
    BundledFile {
        rel: "index.html",
        bytes: include_bytes!("../../../../docs/index.html"),
    },
    BundledFile {
        rel: "README.md",
        bytes: include_bytes!("../../../../docs/README.md"),
    },
    BundledFile {
        rel: "ci.md",
        bytes: include_bytes!("../../../../docs/ci.md"),
    },
    BundledFile {
        rel: "agent-checks.md",
        bytes: include_bytes!("../../../../docs/agent-checks.md"),
    },
    BundledFile {
        rel: "dogfooding.md",
        bytes: include_bytes!("../../../../docs/dogfooding.md"),
    },
    BundledFile {
        rel: "hub.md",
        bytes: include_bytes!("../../../../docs/hub.md"),
    },
    BundledFile {
        rel: "assets/architecture.png",
        bytes: include_bytes!("../../../../docs/assets/architecture.png"),
    },
    BundledFile {
        rel: "assets/graph-propagation.png",
        bytes: include_bytes!("../../../../docs/assets/graph-propagation.png"),
    },
    BundledFile {
        rel: "assets/lifecycle.png",
        bytes: include_bytes!("../../../../docs/assets/lifecycle.png"),
    },
];

/// The topics `claim docs <topic>` accepts, mapped to the bundled page each opens.
///
/// Every entry's target must exist in [`BUNDLE`]; the pairing is asserted in tests
/// so a renamed page cannot leave a topic pointing at a file that is no longer
/// shipped. The overview (`index.html`) is the default when no topic is given.
const TOPICS: &[(&str, &str)] = &[
    ("ci", "ci.md"),
    ("agent-checks", "agent-checks.md"),
    ("dogfooding", "dogfooding.md"),
    ("hub", "hub.md"),
];

/// The machine form of `claim docs`, stable across runs.
#[derive(Debug, Serialize)]
struct DocsReport {
    /// Always `"ok"`; a consumer keys on this rather than the exit code alone.
    status: &'static str,
    /// The absolute path to the page that was selected (the file, not the
    /// directory), so a script can open it itself.
    path: String,
    /// Whether this run asked the platform opener to open the page. `false` without
    /// `--open`, and when `--open` found no opener.
    opened: bool,
}

/// Materialize the bundled site and locate (or, under `--open`, open) the page.
///
/// The page is chosen by `args.topic`: absent means the overview, otherwise the
/// [`TOPICS`] entry for the topic. The default prints only the resolved path
/// (headless-first, so `open "$(claim docs)"` composes); `--open` also asks the
/// platform opener to launch it. Writing the whole bundle every run — not just the
/// requested page — keeps the site's relative links (`assets/*.png`, the inter-page
/// `.md` links) working from the cache directory.
///
/// # Errors
///
/// Fails if the topic is unknown (a usage error naming the valid topics), or if the
/// bundle cannot be written to the cache directory. A missing opener is *not* an
/// error: it degrades to printing the path, because a doc a user can open by hand is
/// not a failure.
pub fn run(args: &DocsArgs, format: Format) -> Result<()> {
    let page_rel = resolve_topic(args.topic.as_deref())?;

    let dir = materialize_bundle().context("could not write the bundled documentation site")?;
    let page = dir.join(page_rel);

    // Headless-first: only `--open` asks the platform opener to launch the page. A
    // box with none (a CI runner, a bare server) is not a failure — the path is
    // printed and the exit stays 0, so the doc is still reachable.
    let opened = if args.open {
        open_in_browser(&page)
    } else {
        false
    };

    // The "no opener found" hint is a human note on stderr, and `note` suppresses it
    // in `--json` mode so a scripted consumer's stderr stays clean while the JSON on
    // stdout (with `opened: false`) already conveys the same fact. Only `--open`
    // (an opener was actually asked for) earns it — the default did not ask to open,
    // so printing the path is the expected outcome, not a warnable failure.
    if args.open && !opened {
        note(
            format,
            "no browser opener was found (open/xdg-open/start); printing the site path.",
        );
    }

    let report = DocsReport {
        status: "ok",
        path: page.display().to_string(),
        opened,
    };

    emit(format, &report, || {
        if opened {
            println!("Opened the docs in your browser:");
            println!("  {}", report.path);
        } else if args.open {
            // `--open` was asked for but no opener was found; still give the path so
            // the doc is reachable by hand.
            println!("Could not open a browser; open this file yourself:");
            println!("  {}", report.path);
        } else {
            // Default: only the path on stdout, so `open "$(claim docs)"` composes.
            println!("{}", report.path);
        }
    })
}

/// Resolve a topic argument to its bundled page path.
///
/// `None` is the overview. A known topic maps to its page; an unknown one is a usage
/// error that lists the valid topics, so a typo is corrected at once rather than
/// silently opening the overview.
fn resolve_topic(topic: Option<&str>) -> Result<&'static str> {
    match topic {
        None => Ok("index.html"),
        Some(name) => TOPICS
            .iter()
            .find(|(t, _)| *t == name)
            .map(|(_, page)| *page)
            .with_context(|| {
                let known = TOPICS
                    .iter()
                    .map(|(t, _)| *t)
                    .collect::<Vec<_>>()
                    .join(", ");
                format!("unknown docs topic '{name}'; valid topics are: {known}")
            }),
    }
}

/// Write every bundled file into a per-version cache directory and return it.
///
/// The directory is version-stamped (`claim-docs/<version>`), so upgrading the
/// binary writes a fresh tree instead of overlaying a new site onto an old one, and
/// two different installed versions never fight over the same files. Files are
/// rewritten every run (cheap for a handful of small files) so a partially written
/// or hand-edited cache self-heals — the bundle in the binary is the source of
/// truth, never whatever happens to be on disk.
///
/// Each file is written atomically (see [`atomic_write`]): the cache is a shared,
/// unsynchronized location, and two `claim docs` runs — or a run and a reader — can
/// touch the same version-stamped path at once. A plain truncate-then-write would let
/// one of them observe a zero-byte or half-written file; the whole content of a file
/// only ever becomes visible at `dest` in a single atomic step, so a torn read is
/// impossible by construction rather than merely unlikely.
fn materialize_bundle() -> Result<PathBuf> {
    let dir = cache_root().join(format!("claim-docs/{}", env!("CARGO_PKG_VERSION")));
    materialize_bundle_into(&dir)?;
    Ok(dir)
}

/// Write the whole bundle under `dir`, each file placed atomically.
///
/// Split from [`materialize_bundle`] so the directory is a parameter: tests exercise
/// the write logic against an isolated temp directory without touching the real
/// per-version cache or mutating process-global environment. The `dir` is the site
/// root; every entry lands at its `rel` path beneath it.
fn materialize_bundle_into(dir: &Path) -> Result<()> {
    for file in BUNDLE {
        let dest = dir.join(file.rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        atomic_write(&dest, file.bytes)
            .with_context(|| format!("could not write {}", dest.display()))?;
    }
    Ok(())
}

/// Write `bytes` to `dest` so a concurrent reader or writer never sees a partial file.
///
/// The bytes are first written in full to a uniquely named temp file *in the same
/// directory* as `dest` — same filesystem, which is what makes the final `rename`
/// atomic — and only then renamed onto `dest`. On Unix, `rename(2)` atomically
/// replaces any existing `dest` with the fully written temp file in one step: another
/// process either sees the old complete file or the new complete file, never a
/// truncated one, and never the zero-byte window that a truncate-in-place
/// (`fs::write`) opens. This is the fix for the docs cache race, where two `claim
/// docs` runs sharing the version-stamped directory could otherwise leave an asset at
/// length 0 while one was mid-write.
///
/// The temp name carries the process id and a per-process counter, so two writers —
/// in the same process or different ones — never pick the same temp path and cannot
/// clobber each other's in-flight file. A crash between writing the temp and renaming
/// it can orphan a `.tmp` file; that is harmless — it is not a `dest` any reader
/// opens, and the cache lives under a sweepable cache directory — so it is left rather
/// than swept.
///
/// On Windows, `fs::rename` refuses to overwrite an existing target, so a `dest`
/// already present makes the rename fail. This function still overwrites — the same
/// self-healing contract as Unix, because the bundle in the binary, not whatever is on
/// disk, is the source of truth: a same-version pre-fix binary wrote `dest` in place
/// and a crash mid-write could have left it 0-byte or partial, so a present `dest` is
/// not assumed good. On that failure it removes `dest` and retries the rename; a
/// genuine error (permission, disk full) propagates rather than being swallowed. The
/// remove/retry has a brief non-atomic window Unix's single `rename` does not, but it
/// never leaves a torn file trusted, and Windows is not a shipped-and-tested target
/// (CI is macOS and Linux); a fully atomic replace there would need a `MoveFileEx`
/// (`MOVEFILE_REPLACE_EXISTING`) syscall and the Windows-only dependency to reach it.
fn atomic_write(dest: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    let parent = dest.parent().unwrap_or_else(|| Path::new("."));
    let file_name = dest
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("docs-file");
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{file_name}.{}.{n}.tmp", std::process::id()));

    fs::write(&tmp, bytes)?;

    let result = match fs::rename(&tmp, dest) {
        Ok(()) => Ok(()),
        Err(_) if cfg!(target_os = "windows") && dest.exists() => {
            // Overwrite unconditionally: drop the present (possibly torn) `dest` and
            // retry, so a crashed pre-fix write cannot be trusted forever.
            fs::remove_file(dest).and_then(|()| fs::rename(&tmp, dest))
        }
        Err(err) => Err(err),
    };

    // On any error arm the temp write did not become `dest`; remove it so a failed
    // run leaves no stray `.tmp` behind. (A crash before this point still can — see
    // the docstring; such orphans are harmless and left for the cache sweep.)
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The base directory for the docs cache, per platform.
///
/// An explicit `CLAIM_DOCS_CACHE_DIR` wins over the platform default when set, giving
/// a caller (notably the test suite) a single, cross-platform way to redirect the
/// cache to an isolated directory so runs never share the real user cache. Otherwise
/// this prefers the OS's user cache location — `$XDG_CACHE_HOME` or `~/.cache` on
/// Linux, `~/Library/Caches` on macOS, `%LOCALAPPDATA%` on Windows — and falls back
/// to the system temp directory when none resolves (an unusual environment with no
/// `HOME`). A cache directory is the right home: the content is reproducible from the
/// binary, so losing it to a cache sweep costs nothing but a rewrite on the next run.
fn cache_root() -> PathBuf {
    if let Some(explicit) = std::env::var_os("CLAIM_DOCS_CACHE_DIR").filter(|v| !v.is_empty()) {
        return PathBuf::from(explicit);
    }
    if cfg!(target_os = "macos") {
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join("Library/Caches");
        }
    } else if cfg!(target_os = "windows") {
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            return PathBuf::from(local);
        }
    } else {
        if let Some(xdg) = std::env::var_os("XDG_CACHE_HOME").filter(|v| !v.is_empty()) {
            return PathBuf::from(xdg);
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".cache");
        }
    }
    std::env::temp_dir()
}

/// Ask the platform opener to open `page`, returning whether it launched.
///
/// Uses `open` on macOS, `xdg-open` on other Unix, and `cmd /C start` on Windows.
/// A `false` return (the opener is absent or exits non-zero, as on a headless box)
/// is not an error the caller must handle by failing — [`run`] degrades to printing
/// the path — so this reports success as a bool rather than a `Result`.
///
/// The opener's own stdout and stderr are discarded: this verb's stdout contract is
/// one JSON object (or one path line) and nothing else, so a chatty `xdg-open` must
/// never leak into what a `--json` consumer parses.
fn open_in_browser(page: &Path) -> bool {
    let mut command = if cfg!(target_os = "macos") {
        let mut c = Command::new("open");
        c.arg(page);
        c
    } else if cfg!(target_os = "windows") {
        // `start` is a `cmd` builtin, not a program; the empty title argument keeps
        // a quoted path from being consumed as the window title.
        let mut c = Command::new("cmd");
        c.args(["/C", "start", ""]).arg(page);
        c
    } else {
        let mut c = Command::new("xdg-open");
        c.arg(page);
        c
    };

    let launched = command
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status();

    matches!(launched, Ok(status) if status.success())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_topic_maps_to_a_bundled_page() {
        // A topic pointing at a page that is not shipped would open a broken site.
        for (topic, page) in TOPICS {
            assert!(
                BUNDLE.iter().any(|f| f.rel == *page),
                "topic '{topic}' -> '{page}' is not in the bundle"
            );
        }
    }

    #[test]
    fn the_overview_and_its_assets_are_bundled() {
        // The default page and every image it references must ship, or `claim docs`
        // opens an overview with broken diagrams.
        for rel in [
            "index.html",
            "assets/architecture.png",
            "assets/graph-propagation.png",
            "assets/lifecycle.png",
        ] {
            assert!(
                BUNDLE.iter().any(|f| f.rel == rel),
                "{rel} is missing from the bundle"
            );
        }
    }

    #[test]
    fn no_bundled_file_is_empty() {
        // A zero-byte embed means a moved source file that `include_bytes!` still
        // found by name but that no longer has content — caught here, not in the
        // opened browser.
        for file in BUNDLE {
            assert!(!file.bytes.is_empty(), "{} is empty", file.rel);
        }
    }

    /// The bytes of a bundled file by its site-relative path, for tests that read the
    /// embedded HTML rather than a materialized copy.
    fn bundled(rel: &str) -> &'static [u8] {
        BUNDLE
            .iter()
            .find(|f| f.rel == rel)
            .unwrap_or_else(|| panic!("{rel} is not bundled"))
            .bytes
    }

    /// Every local `src`/`href` target the given HTML references — the value inside
    /// `src="..."` or `href="..."` — minus external URLs and in-page anchors, so a
    /// future `<img>` or `<a>` pointing at an unbundled file is visible to the caller.
    fn referenced_local_targets(html: &str) -> Vec<String> {
        let mut out = Vec::new();
        for attr in ["src=\"", "href=\""] {
            let mut rest = html;
            while let Some(start) = rest.find(attr) {
                rest = &rest[start + attr.len()..];
                let Some(end) = rest.find('"') else { break };
                let value = &rest[..end];
                rest = &rest[end + 1..];
                // Skip absolute URLs and pure in-page anchors; keep only relative
                // references to files that must travel in the bundle.
                if value.is_empty()
                    || value.starts_with('#')
                    || value.contains("://")
                    || value.starts_with("mailto:")
                {
                    continue;
                }
                // Drop any `#fragment` on an otherwise-local link (`ci.md#lanes`).
                let path = value.split('#').next().unwrap_or(value);
                if !path.is_empty() {
                    out.push(path.to_owned());
                }
            }
        }
        out
    }

    #[test]
    fn every_local_reference_in_the_overview_is_bundled() {
        // The regression this guards: someone adds an `<img src="assets/new.png">` or
        // a link to a new topic page to index.html but forgets to add the file to
        // BUNDLE. `claim docs` would then open a site with a broken image or dead
        // link — silent doc-rot, exactly what this project exists to prevent. Every
        // local target the overview references must be a file the binary ships.
        let html = std::str::from_utf8(bundled("index.html")).expect("index.html is UTF-8");
        let targets = referenced_local_targets(html);
        assert!(
            !targets.is_empty(),
            "expected the overview to reference at least the diagram images"
        );
        for target in targets {
            assert!(
                BUNDLE.iter().any(|f| f.rel == target),
                "index.html references '{target}', which is not in BUNDLE — add it to the \
                 bundle or the shipped site has a broken link"
            );
        }
    }

    #[test]
    fn resolve_topic_maps_known_and_default() {
        assert_eq!(resolve_topic(None).unwrap(), "index.html");
        assert_eq!(resolve_topic(Some("ci")).unwrap(), "ci.md");
        assert_eq!(
            resolve_topic(Some("agent-checks")).unwrap(),
            "agent-checks.md"
        );
        assert_eq!(resolve_topic(Some("dogfooding")).unwrap(), "dogfooding.md");
    }

    #[test]
    fn resolve_topic_rejects_unknown_naming_the_valid_ones() {
        let err = resolve_topic(Some("nope")).unwrap_err().to_string();
        assert!(err.contains("nope"), "names the bad topic: {err}");
        assert!(err.contains("ci"), "lists the valid topics: {err}");
    }

    #[test]
    fn materialize_writes_the_whole_site_with_resolving_links() {
        // The bundle must land on disk as a real site: the overview present, the
        // referenced images present under assets/, and the bytes intact — so the
        // relative links in the HTML resolve to files that exist. Written into an
        // isolated temp dir so the test neither touches nor depends on the real
        // per-version user cache.
        let root = tempfile::TempDir::new().unwrap();
        let dir = root.path();
        materialize_bundle_into(dir).unwrap();
        let index = dir.join("index.html");
        assert!(index.is_file(), "index.html was not written");

        let html = fs::read_to_string(&index).unwrap();
        assert!(html.contains("<title>claim"), "index.html looks wrong");

        for rel in [
            "assets/architecture.png",
            "assets/graph-propagation.png",
            "assets/lifecycle.png",
            "ci.md",
            "agent-checks.md",
            "dogfooding.md",
            "hub.md",
        ] {
            let path = dir.join(rel);
            let meta = fs::metadata(&path).unwrap_or_else(|_| panic!("{rel} was not written"));
            assert!(meta.len() > 0, "{rel} was written empty");
        }
    }

    #[test]
    fn concurrent_materialize_into_one_dir_never_tears_a_file() {
        // The docs-cache race, in-process and reduced to its mechanism: writers
        // hammer the shared directory while readers spin, stat, and read each asset.
        // With the old truncate-in-place `fs::write`, an asset spends a real window at
        // length 0 (`O_TRUNC` zeroes it before the ~250 KB PNG is written back), and a
        // reader that stats in that window sees `len == 0` — exactly the
        // `architecture.png must not be empty` panic. The `len != 0` assertion below is
        // what catches the bug: it is the same invariant the original test tripped, and
        // the mechanism can only ever leave an asset short (0 or a growing prefix),
        // never full-length-with-wrong-bytes. The inline byte-compare on a full-length
        // read is a cheap extra guard, not the real content check — with truncate it
        // can effectively only fire alongside a completed write; the post-storm
        // full-bundle byte-check is where content is truly verified. Atomic rename makes
        // both impossible: the asset appears at `dest` only as a complete file.
        let root = tempfile::TempDir::new().unwrap();
        let dir = root.path().to_path_buf();

        // Seed the tree once so a reader that stats an asset is testing its content,
        // not racing its first appearance.
        materialize_bundle_into(&dir).unwrap();

        let assets = [
            "assets/architecture.png",
            "assets/graph-propagation.png",
            "assets/lifecycle.png",
            "index.html",
        ];
        let done = std::sync::atomic::AtomicBool::new(false);

        std::thread::scope(|scope| {
            // Writers: keep re-materializing to widen the number of truncate windows a
            // reader can fall into.
            for _ in 0..8 {
                let dir = dir.clone();
                scope.spawn(move || {
                    for _ in 0..60 {
                        materialize_bundle_into(&dir).unwrap();
                    }
                });
            }
            // Readers: spin until the writers finish, checking each asset every pass.
            for _ in 0..4 {
                let dir = dir.clone();
                let done = &done;
                scope.spawn(move || {
                    while !done.load(std::sync::atomic::Ordering::Relaxed) {
                        for rel in assets {
                            let want = bundled(rel);
                            match fs::metadata(dir.join(rel)) {
                                Ok(meta) => assert_ne!(
                                    meta.len(),
                                    0,
                                    "{rel} observed empty mid-write — the truncate race"
                                ),
                                Err(_) => continue,
                            }
                            if let Ok(contents) = fs::read(dir.join(rel)) {
                                if contents.len() == want.len() {
                                    assert_eq!(
                                        contents, want,
                                        "{rel} read at full length but wrong bytes — a torn read"
                                    );
                                }
                            }
                        }
                    }
                });
            }
            // One more writer whose end signals the readers to stop; the scope joins
            // every thread, so the readers exit their spin once the writes are done.
            {
                let dir = dir.clone();
                let done = &done;
                scope.spawn(move || {
                    for _ in 0..60 {
                        materialize_bundle_into(&dir).unwrap();
                    }
                    done.store(true, std::sync::atomic::Ordering::Relaxed);
                });
            }
        });

        // After the storm every asset must hold exactly the embedded bytes.
        for file in BUNDLE {
            let on_disk = fs::read(dir.join(file.rel))
                .unwrap_or_else(|_| panic!("{} was not written", file.rel));
            assert_eq!(
                on_disk, file.bytes,
                "{} does not match the embedded source after concurrent writes",
                file.rel
            );
        }
    }

    #[test]
    fn atomic_write_overwrites_existing_and_leaves_no_temp() {
        // Every run overwrites `dest` wholesale — the self-healing contract on both
        // platforms, so a torn `dest` a crashed pre-fix write left behind is replaced,
        // never trusted. A pre-seeded 0-byte `dest` (the exact crash-recovery shape the
        // Windows overwrite path exists for) must be replaced by the full bytes; and
        // atomic_write must leave no `.tmp` artifact, so the cache holds only the
        // materialized files.
        let root = tempfile::TempDir::new().unwrap();
        let dest = root.path().join("page.html");

        // Stand in for a crash-truncated `dest` from an earlier, pre-fix run.
        fs::write(&dest, b"").unwrap();
        assert_eq!(fs::metadata(&dest).unwrap().len(), 0);

        atomic_write(&dest, b"first").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"first");

        atomic_write(&dest, b"second-and-longer").unwrap();
        assert_eq!(fs::read(&dest).unwrap(), b"second-and-longer");

        let strays: Vec<_> = fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .collect();
        assert!(strays.is_empty(), "atomic_write left a temp file behind");
    }
}
