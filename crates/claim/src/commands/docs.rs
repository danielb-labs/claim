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
//! normal local site) and handed to the platform's file opener. `--path` prints the
//! path without opening, for headless and scripting use, and a missing opener (a CI
//! box, a bare server) degrades to printing the path and exiting `0` rather than
//! failing — a doc you can't auto-open is still a doc you can `open` yourself.

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
];

/// The machine form of `claim docs`, stable across runs.
#[derive(Debug, Serialize)]
struct DocsReport {
    /// Always `"ok"`; a consumer keys on this rather than the exit code alone.
    status: &'static str,
    /// The absolute path to the page that was selected (the file, not the
    /// directory), so a script can open it itself.
    path: String,
    /// Whether this run asked the platform opener to open the page. `false` for
    /// `--path` and for a headless environment where no opener was found.
    opened: bool,
}

/// Materialize the bundled site and open (or just locate) the requested page.
///
/// The page is chosen by `args.topic`: absent means the overview, otherwise the
/// [`TOPICS`] entry for the topic. `--path` (or a headless box with no opener) only
/// prints the resolved path; the default opens it with the platform opener. Writing
/// the whole bundle every run — not just the requested page — keeps the site's
/// relative links (`assets/*.png`, the inter-page `.md` links) working from the
/// cache directory.
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

    // `--path` never opens, for headless and scripting use. Otherwise attempt the
    // platform opener; a box with none (a CI runner, a bare server) is not a
    // failure — we print the path and exit 0, so the doc is still reachable.
    let opened = if args.path {
        false
    } else {
        open_in_browser(&page)
    };

    // The "no opener found" hint goes to stderr before the result on stdout; the two
    // streams are independent, so a `--json` or `--path` consumer parsing stdout is
    // unaffected by the ordering. Only the default (open-attempted) headless case
    // earns it — `--path` explicitly did not ask to open.
    if !opened && !args.path {
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
        } else if args.path {
            // `--path` prints only the path on stdout, so it composes:
            // `open "$(claim docs --path)"`.
            println!("{}", report.path);
        } else {
            println!("Could not open a browser; open this file yourself:");
            println!("  {}", report.path);
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
fn materialize_bundle() -> Result<PathBuf> {
    let dir = cache_root().join(format!("claim-docs/{}", env!("CARGO_PKG_VERSION")));

    for file in BUNDLE {
        let dest = dir.join(file.rel);
        if let Some(parent) = dest.parent() {
            fs::create_dir_all(parent)
                .with_context(|| format!("could not create {}", parent.display()))?;
        }
        fs::write(&dest, file.bytes)
            .with_context(|| format!("could not write {}", dest.display()))?;
    }

    Ok(dir)
}

/// The base directory for the docs cache, per platform.
///
/// Prefers the OS's user cache location — `$XDG_CACHE_HOME` or `~/.cache` on Linux,
/// `~/Library/Caches` on macOS, `%LOCALAPPDATA%` on Windows — and falls back to the
/// system temp directory when none resolves (an unusual environment with no `HOME`).
/// A cache directory is the right home: the content is reproducible from the binary,
/// so losing it to a cache sweep costs nothing but a rewrite on the next run.
fn cache_root() -> PathBuf {
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
        // relative links in the HTML resolve to files that exist.
        let dir = materialize_bundle().unwrap();
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
        ] {
            let path = dir.join(rel);
            let meta = fs::metadata(&path).unwrap_or_else(|_| panic!("{rel} was not written"));
            assert!(meta.len() > 0, "{rel} was written empty");
        }
    }
}
