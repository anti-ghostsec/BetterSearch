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

/// Wisp's WebKit cookie stores, which exist only with its persistent profile on.
/// Wisp keeps no plaintext history, so this is the only file of its that says
/// anything about where you've been.
fn wisp_dbs() -> Vec<PathBuf> {
	let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
	let dir = home.join(".local/share/wisp/profile");
	["cookies.db", "cookies.sqlite", "cookies.sqlite3"].iter().map(|n| dir.join(n)).filter(|p| p.is_file()).collect()
}

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

/// The UA Wisp would use: its own setting if one is set, else a WebKit one.
/// A clearance cookie is bound to it, so it has to go out with the cookie.
fn wisp_ua() -> String {
	let home = std::env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
	let set = std::fs::read_to_string(home.join(".config/wisp/config.json"))
		.ok()
		.and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
		.and_then(|v| v["user_agent"].as_str().map(str::to_string))
		.filter(|u| !u.trim().is_empty());
	set.unwrap_or_else(|| "Mozilla/5.0 (X11; Linux x86_64) AppleWebKit/605.1.15 (KHTML, like Gecko) Version/17.0 Safari/605.1.15".into())
}

/// A new, empty folder with mode 0700. Created, never reused: if the name is
/// taken (someone else made it first), there's no copy at all.
fn private_dir() -> Option<PathBuf> {
	use std::os::unix::fs::DirBuilderExt;
	let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
	let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.subsec_nanos()).unwrap_or(0);
	let dir = base.join(format!("bs-web-{}-{nanos:08x}", std::process::id()));
	std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
	Some(dir)
}

/// Cookies for a host out of a WebKit store. If the immutable open finds none
/// (recent cookies may still be in the WAL), the files are copied aside first.
fn from_webkit(db: &Path, host: &str) -> String {
	let query = "SELECT name, value FROM cookies WHERE domain = ?1 OR domain = ?2";
	let read = |conn: &Connection| -> Vec<(String, String)> {
		let Ok(mut st) = conn.prepare(query) else { return Vec::new() };
		let Ok(rows) = st.query_map([host, &format!(".{host}")], |r| Ok((r.get(0)?, r.get(1)?))) else { return Vec::new() };
		rows.flatten().collect()
	};
	if let Some(conn) = open(db) {
		let rows = read(&conn);
		if !rows.is_empty() {
			return pairs(rows);
		}
	}
	// Recent cookies may only be in the WAL; with none there, nothing is newer.
	let wal = PathBuf::from(format!("{}-wal", db.display()));
	if !std::fs::metadata(&wal).is_ok_and(|m| m.len() > 0) {
		return String::new();
	}
	// Copy the set aside so the WAL is read too. The copy holds every site's
	// cookies, so it goes in a fresh folder only this user can open, in RAM
	// when the session has a runtime directory, and is removed straight after.
	let Some(tmp) = private_dir() else { return String::new() };
	let copied = tmp.join("cookies.db");
	let ok = std::fs::copy(db, &copied).is_ok() && std::fs::copy(&wal, tmp.join("cookies.db-wal")).is_ok();
	let rows = if ok {
		Connection::open(&copied).ok().map(|c| read(&c)).unwrap_or_default()
	} else {
		Vec::new()
	};
	let _ = std::fs::remove_dir_all(&tmp);
	pairs(rows)
}

/// Clearance first, so a server reading only the first cookie sees it.
fn pairs(mut rows: Vec<(String, String)>) -> String {
	rows.retain(|(_, v)| !v.is_empty());
	rows.sort_by_key(|(n, _)| !CLEARANCE.contains(&n.as_str()));
	rows.into_iter().map(|(n, v)| format!("{n}={v}")).collect::<Vec<_>>().join("; ")
}

/// The best cookie header each of `hosts` has, and how many cookies it holds.
/// Profiles that know nothing about a host are skipped.
pub fn harvest(hosts: &[String]) -> Vec<(String, String, String, usize)> {
	let profiles = profiles();
	let wisp = wisp_dbs();
	let wisp_ua = wisp_ua();
	let mut out = Vec::new();
	for host in hosts.iter().filter(|h| !h.is_empty()) {
		let mut best = (String::new(), String::new(), 0usize);
		let offer = |header: String, ua: &str, best: &mut (String, String, usize)| {
			let n = header.matches('=').count();
			if n > best.2 {
				*best = (header, ua.to_string(), n);
			}
		};
		for db in &wisp {
			offer(from_webkit(db, host), &wisp_ua, &mut best);
		}
		for db in &profiles {
			offer(from_profile(db, host), bettersearch_core::sites::firefox_ua(), &mut best);
		}
		if !best.0.is_empty() {
			out.push((host.clone(), best.0, best.1, best.2));
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
	fn reads_a_webkit_store() {
		let dir = std::env::temp_dir().join(format!("bs-webkit-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let db = dir.join("cookies.db");
		let conn = Connection::open(&db).unwrap();
		conn.execute_batch(
			"CREATE TABLE cookies (name TEXT, value TEXT, domain TEXT, path TEXT);
			 INSERT INTO cookies VALUES ('cf_clearance','abc','.x.test','/');
			 INSERT INTO cookies VALUES ('other','1','.elsewhere','/');",
		)
		.unwrap();
		drop(conn);
		assert_eq!(from_webkit(&db, "x.test"), "cf_clearance=abc");
		assert_eq!(from_webkit(&db, "nothing.test"), "");
		std::fs::remove_dir_all(&dir).unwrap();
	}

	/// A cookie still in the WAL of a running browser is found through a
	/// private copy, and the copy is gone afterwards.
	#[test]
	fn reads_a_cookie_still_in_the_wal() {
		let dir = std::env::temp_dir().join(format!("bs-webkit-wal-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let db = dir.join("cookies.db");
		let conn = Connection::open(&db).unwrap();
		conn.execute_batch(
			"PRAGMA journal_mode=WAL; PRAGMA wal_autocheckpoint=0;
			 CREATE TABLE cookies (name TEXT, value TEXT, domain TEXT, path TEXT);
			 INSERT INTO cookies VALUES ('cf_clearance','fresh','.wal.test','/');",
		)
		.unwrap();
		assert!(std::fs::metadata(dir.join("cookies.db-wal")).unwrap().len() > 0);
		assert_eq!(from_webkit(&db, "wal.test"), "cf_clearance=fresh");
		let base = std::env::var_os("XDG_RUNTIME_DIR").map(PathBuf::from).filter(|d| d.is_dir()).unwrap_or_else(std::env::temp_dir);
		let prefix = format!("bs-web-{}-", std::process::id());
		let left = std::fs::read_dir(base).unwrap().flatten().filter(|e| e.file_name().to_string_lossy().starts_with(&prefix)).count();
		assert_eq!(left, 0, "the private copy was left behind");
		drop(conn);
		std::fs::remove_dir_all(&dir).unwrap();
	}

	#[test]
	fn host_extraction() {
		assert_eq!(host_of("https://booru.example/x"), "booru.example");
		assert_eq!(host_of("https://booru.example:8443/x"), "booru.example");
	}
}
