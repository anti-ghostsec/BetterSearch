//! Borrowing the browser's clearance cookies.
//!
//! Some boorus put an interactive challenge in front of everything; a plain
//! client gets HTML forever. There's no browser in this app, but there is one
//! on the machine: visit the site there once, and the clearance cookie it
//! earned is read out of that profile and kept in the vault. The browser's
//! database is opened read-only and immutable, never written.

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags};

/// The cookies that matter for a challenge. Everything else for the host goes
/// along too, because the site's own session is usually needed as well.
pub const CLEARANCE: &[&str] = &["cf_clearance", "__cf_bm", "cf_chl_2", "cf_chl_prog", "cf_chl_seq"];

/// Every `cookies.sqlite` a Firefox-family browser keeps on this machine.
pub fn profiles() -> Vec<PathBuf> {
	let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
	let mut roots = vec![
		home.join(".librewolf"),
		home.join(".mozilla/firefox"),
		home.join(".config/librewolf"),
		home.join(".var/app/io.gitlab.librewolf-community/.librewolf"),
		home.join(".var/app/org.mozilla.firefox/.mozilla/firefox"),
	];
	roots.retain(|r| r.is_dir());
	let mut out = Vec::new();
	for root in roots {
		let Ok(entries) = std::fs::read_dir(&root) else { continue };
		for entry in entries.flatten() {
			let db = entry.path().join("cookies.sqlite");
			if db.is_file() {
				out.push(db);
			}
		}
		// A profile folder directly under the root is unusual but harmless.
	}
	out
}

fn open(db: &std::path::Path) -> Option<Connection> {
	// URI with `immutable`, so a running browser's WAL isn't consulted and the
	// file is never locked or changed.
	let path = db.to_string_lossy().replace(' ', "%20");
	Connection::open_with_flags(
		format!("file:{path}?immutable=1"),
		OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
	)
	.ok()
}

/// Cookies for one host out of one profile, as a request header.
fn from_profile(db: &Path, host: &str) -> String {
	let Some(conn) = open(db) else { return String::new() };
	let Ok(mut st) = conn.prepare("SELECT name, value FROM moz_cookies WHERE host = ?1 OR host = ?2") else { return String::new() };
	let rows = match st.query_map([host, &format!(".{host}")], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))) {
		Ok(rows) => rows,
		Err(_) => return String::new(),
	};
	let mut pairs: Vec<(String, String)> = rows.flatten().filter(|(_, v)| !v.is_empty()).collect();
	// Clearance first: a server that only reads the first cookie still sees it.
	pairs.sort_by_key(|(n, _)| !CLEARANCE.contains(&n.as_str()));
	pairs.into_iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("; ")
}

/// The best cookie header each of `hosts` has, and how many cookies it holds.
/// Profiles that know nothing about a host are skipped.
pub fn harvest(hosts: &[String]) -> Vec<(String, String, usize)> {
	let profiles = profiles();
	let mut out = Vec::new();
	for host in hosts.iter().filter(|h| !h.is_empty()) {
		let mut best = (String::new(), 0usize);
		for db in &profiles {
			let header = from_profile(db, host);
			let n = header.matches('=').count();
			if n > best.1 {
				best = (header, n);
			}
		}
		if !best.0.is_empty() {
			out.push((host.clone(), best.0, best.1));
		}
	}
	out
}

/// Host for cookie lookups from a site's address.
pub fn host_of(base: &str) -> String {
	bettersearch_core::sites::host_of(base).unwrap_or_default().to_lowercase()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn profiles_are_only_real_files() {
		for p in profiles() {
			assert!(p.is_file(), "{}", p.display());
		}
	}

	#[test]
	fn a_missing_host_harvests_nothing() {
		assert!(harvest(&["nope.invalid".into()]).is_empty());
	}

	#[test]
	fn host_extraction() {
		assert_eq!(host_of("https://booru.example/x"), "booru.example");
		assert_eq!(host_of("https://booru.example:8443/x"), "booru.example");
	}
}
