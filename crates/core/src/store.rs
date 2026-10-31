// Everything kept on this computer, in one SQLite database: the tag graph,
// posts seen (so links can be searched without asking a site), translations,
// search history and settings. The app keeps it in memory only and writes it
// out encrypted (`to_bytes` / `from_bytes`); `open` on a file is for tools.

use std::collections::HashMap;
use std::path::Path;

use rusqlite::{Connection, MAIN_DB, OptionalExtension, params};
use zeroize::Zeroizing;

use crate::post::Post;
use crate::query::{Expr, Subject, compare, parse};
use crate::taggraph::{ItemLink, LinkSuggestion, Options, TagGraph};
use crate::{Error, Rating, Result, wipe};

pub struct Store {
	conn: Connection,
	opts: Options,
	/// Who votes from this computer.
	pub actor: String,
	/// Site aliases looked up this run: (site, name) → current name, or None for "not an alias".
	aliases: HashMap<(String, String), Option<String>>,
	/// Fill in links made before on posts as they arrive (see `auto_link`).
	pub auto_links: bool,
}

/// Who fills in links on their own, so the window can tell them apart.
pub const AUTO: &str = "auto";

/// Groups are kept as items of their own, with their members as tags: "group:animal".
const GROUP: &str = "group:";
/// Relations between tags (rabbit eat carrot) are links on this item, so they
/// fill in on every post with those tags.
pub const RULES: &str = "rules";
/// Most links one `kind[@a @b]` may stand for.
const MAX_COMBOS: usize = 64;

/// Posts kept in the cache besides ones with links or tags of your own.
const KEEP_POSTS: usize = 5000;

impl Store {
	pub fn open(path: &Path) -> Result<Store> {
		Self::with(wipe::open(path)?)
	}

	pub fn in_memory() -> Result<Store> {
		Self::with(wipe::open_in_memory()?)
	}

	/// A database from `to_bytes`, held in memory.
	pub fn from_bytes(bytes: &[u8]) -> Result<Store> {
		let mut conn = wipe::open_in_memory()?;
		conn.deserialize_read_exact(MAIN_DB, bytes, bytes.len(), false)?;
		let store = Self::with(conn)?;
		// Rewrites the database without its free pages, so anything deleted
		// before secure delete was on is gone from the next save.
		store.conn.execute_batch("VACUUM")?;
		Ok(store)
	}

	/// The whole database as bytes, for writing out (encrypted) and reading
	/// back with `from_bytes`. Wiped from memory when dropped.
	pub fn to_bytes(&mut self) -> Result<Zeroizing<Vec<u8>>> {
		Ok(Zeroizing::new(self.conn.serialize(MAIN_DB)?.to_vec()))
	}

	/// Reads a database file from before it was kept in memory.
	pub fn import_file(path: &Path) -> Result<Store> {
		let mut conn = wipe::open(path)?;
		// Folds the WAL in; a WAL-mode copy won't open from bytes.
		conn.execute_batch("PRAGMA journal_mode = DELETE")?;
		let bytes = Zeroizing::new(conn.serialize(MAIN_DB)?.to_vec());
		drop(conn);
		Self::from_bytes(&bytes)
	}

	/// Rows changed since opening; a different number means there's something to save.
	pub fn changes(&self) -> u64 {
		self.conn.total_changes()
	}

	fn with(conn: Connection) -> Result<Store> {
		conn.execute_batch(
			"PRAGMA foreign_keys = ON;
			PRAGMA secure_delete = ON;
			-- Sorts, temporary tables and VACUUM's working copy stay in memory,
			-- never in temporary files on disk.
			PRAGMA temp_store = MEMORY;
			CREATE TABLE IF NOT EXISTS posts (
				key TEXT PRIMARY KEY,
				site TEXT NOT NULL,
				id INTEGER NOT NULL,
				json TEXT NOT NULL,
				seen_at INTEGER NOT NULL
			);
			CREATE TABLE IF NOT EXISTS translations (
				site TEXT NOT NULL,
				tag TEXT NOT NULL,
				target TEXT NOT NULL,
				PRIMARY KEY (site, tag)
			);
			CREATE TABLE IF NOT EXISTS kv (key TEXT PRIMARY KEY, value TEXT NOT NULL);
			CREATE TABLE IF NOT EXISTS favorites (
				key TEXT PRIMARY KEY,
				site TEXT NOT NULL,
				id INTEGER NOT NULL,
				at INTEGER NOT NULL
			);
			CREATE TABLE IF NOT EXISTS relation_index (
				subject TEXT NOT NULL,
				verb TEXT NOT NULL,
				object TEXT NOT NULL,
				score REAL NOT NULL,
				state TEXT NOT NULL DEFAULT 'learned',
				site TEXT NOT NULL,
				at INTEGER NOT NULL,
				PRIMARY KEY (subject, verb, object)
			);
			CREATE TABLE IF NOT EXISTS relation_learned (
				subject TEXT NOT NULL,
				verb TEXT NOT NULL,
				site TEXT NOT NULL,
				at INTEGER NOT NULL,
				PRIMARY KEY (subject, verb)
			);
			CREATE TABLE IF NOT EXISTS saved_searches (
				id INTEGER PRIMARY KEY,
				name TEXT NOT NULL UNIQUE,
				query TEXT NOT NULL,
				sites TEXT NOT NULL,
				ratings TEXT NOT NULL,
				seen TEXT NOT NULL DEFAULT '{}',
				created_at INTEGER NOT NULL,
				run_at INTEGER
			);
			CREATE TABLE IF NOT EXISTS cookies (
				host TEXT PRIMARY KEY,
				header TEXT NOT NULL,
				at INTEGER NOT NULL
			);
			CREATE TABLE IF NOT EXISTS history (
				id INTEGER PRIMARY KEY,
				query TEXT NOT NULL,
				sites TEXT NOT NULL,
				at INTEGER NOT NULL
			);",
		)?;
		let opts = Options::default();
		TagGraph::migrate(&conn, &opts)?;
		Ok(Store { conn, opts, actor: "local".into(), aliases: HashMap::new(), auto_links: true })
	}

	pub fn graph(&self) -> TagGraph<'_> {
		TagGraph::new(&self.conn, &self.opts)
	}

	/// The raw connection, for queries that don't need the graph.
	pub fn conn_ref(&self) -> &Connection {
		&self.conn
	}

	pub fn save_posts(&self, posts: &[Post]) -> Result<()> {
		let tx = self.conn.unchecked_transaction()?;
		{
			let mut st = tx.prepare_cached(
				"INSERT INTO posts (key, site, id, json, seen_at) VALUES (?, ?, ?, ?, unixepoch())
				ON CONFLICT (key) DO UPDATE SET json = excluded.json, seen_at = excluded.seen_at",
			)?;
			for p in posts {
				st.execute(params![p.key(), p.site, p.id as i64, serde_json::to_string(p).expect("posts serialize")])?;
			}
		}
		tx.commit()?;
		Ok(())
	}

	pub fn post(&self, key: &str) -> Result<Option<Post>> {
		let json: Option<String> = self.conn.query_row("SELECT json FROM posts WHERE key = ?", [key], |r| r.get(0)).optional()?;
		Ok(json.and_then(|j| serde_json::from_str(&j).ok()))
	}

	/// Saved posts from one site that have a link, newest first.
	pub fn linked_posts(&self, site: &str) -> Result<Vec<Post>> {
		let mut st = self.conn.prepare_cached(&self.graph_sql(
			"SELECT p.json FROM posts p WHERE p.site = ? AND EXISTS (SELECT 1 FROM {links} l WHERE l.item = p.key) ORDER BY p.id DESC",
		))?;
		let rows = st.query_map([site], |r| r.get::<_, String>(0))?;
		Ok(rows.filter_map(|r| r.ok().and_then(|j| serde_json::from_str(&j).ok())).collect())
	}

	/// Favorites a post (keeping a copy of it, so it's there offline and never
	/// pruned) or takes it out of your favorites.
	pub fn set_favorite(&self, post: &Post, on: bool) -> Result<()> {
		if on {
			self.save_posts(std::slice::from_ref(post))?;
			self.conn.execute(
				"INSERT INTO favorites (key, site, id, at) VALUES (?, ?, ?, unixepoch()) ON CONFLICT (key) DO NOTHING",
				params![post.key(), post.site, post.id as i64],
			)?;
		} else {
			self.conn.execute("DELETE FROM favorites WHERE key = ?", [post.key()])?;
		}
		Ok(())
	}

	pub fn is_favorite(&self, key: &str) -> Result<bool> {
		Ok(self.conn.query_row("SELECT 1 FROM favorites WHERE key = ?", [key], |_| Ok(())).optional()?.is_some())
	}

	/// Every favorite's key, to mark them in the grid.
	pub fn favorite_keys(&self) -> Result<std::collections::HashSet<String>> {
		let mut st = self.conn.prepare_cached("SELECT key FROM favorites")?;
		let rows = st.query_map([], |r| r.get::<_, String>(0))?;
		Ok(rows.collect::<rusqlite::Result<_>>()?)
	}

	/// One site's favorites, newest post first.
	pub fn favorite_posts(&self, site: &str) -> Result<Vec<Post>> {
		let mut st = self
			.conn
			.prepare_cached("SELECT p.json FROM favorites f JOIN posts p ON p.key = f.key WHERE f.site = ? ORDER BY p.id DESC")?;
		let rows = st.query_map([site], |r| r.get::<_, String>(0))?;
		Ok(rows.filter_map(|r| r.ok().and_then(|j| serde_json::from_str(&j).ok())).collect())
	}

	/// Borrowed browser cookies, by host.
	pub fn set_cookie(&self, host: &str, header: &str) -> Result<()> {
		self.conn.execute(
			"INSERT INTO cookies (host, header, at) VALUES (?, ?, unixepoch())
			ON CONFLICT (host) DO UPDATE SET header = excluded.header, at = excluded.at",
			rusqlite::params![host, header],
		)?;
		Ok(())
	}

	pub fn cookies(&self) -> Result<Vec<(String, String, i64)>> {
		let mut st = self.conn.prepare_cached("SELECT host, header, at FROM cookies")?;
		let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
		Ok(rows.collect::<rusqlite::Result<_>>()?)
	}

	pub fn delete_cookie(&self, host: &str) -> Result<()> {
		self.conn.execute("DELETE FROM cookies WHERE host = ?", [host])?;
		Ok(())
	}

	/// Every favorite's tags with when it was favorited, for taste scoring.
	pub fn favorite_tag_events(&self) -> Result<Vec<(String, i64)>> {
		let mut st = self.conn.prepare_cached("SELECT p.json, f.at FROM favorites f JOIN posts p ON p.key = f.key")?;
		let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
		let mut out = Vec::new();
		for row in rows {
			let (json, at) = row?;
			if let Ok(post) = serde_json::from_str::<Post>(&json) {
				out.extend(post.tags.into_iter().map(|t| (t, at)));
			}
		}
		Ok(out)
	}

	fn graph_sql(&self, text: &str) -> String {
		text.replace("{links}", &format!("{}_links", self.opts.prefix)).replace("{item_tags}", &format!("{}_item_tags", self.opts.prefix))
	}

	/// A stored value, such as the app's settings as JSON.
	pub fn value(&self, key: &str) -> Result<Option<String>> {
		Ok(self.conn.query_row("SELECT value FROM kv WHERE key = ?", [key], |r| r.get(0)).optional()?)
	}

	pub fn set_value(&self, key: &str, value: &str) -> Result<()> {
		self.conn.execute(
			"INSERT INTO kv (key, value) VALUES (?, ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value",
			params![key, value],
		)?;
		Ok(())
	}

	/// Remembers a search that ran. Running the same one again moves it to the top.
	pub fn add_history(&self, query: &str, sites: &[String]) -> Result<()> {
		let query = query.trim();
		if query.is_empty() {
			return Ok(());
		}
		self.conn.execute("DELETE FROM history WHERE query = ?", [query])?;
		self.conn.execute("INSERT INTO history (query, sites, at) VALUES (?, ?, unixepoch())", params![query, sites.join(",")])?;
		Ok(())
	}

	/// Recent searches, newest first, with when they ran (Unix seconds).
	pub fn history(&self, limit: usize) -> Result<Vec<(String, i64)>> {
		let mut st = self.conn.prepare_cached("SELECT query, at FROM history ORDER BY id DESC LIMIT ?")?;
		let rows = st.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?;
		Ok(rows.collect::<rusqlite::Result<_>>()?)
	}

	/// Tags used in past searches, most used first: (tag, searches it was in).
	pub fn history_tags(&self, limit: usize) -> Result<Vec<(String, usize)>> {
		let mut counts: HashMap<String, usize> = HashMap::new();
		for (q, _) in self.history(500)? {
			if let Ok(e) = parse(&q) {
				let mut seen = Vec::new();
				e.walk(&mut |e| {
					if let Expr::Tag(t) = e
						&& !seen.contains(t)
					{
						seen.push(t.clone());
					}
				});
				for t in seen {
					*counts.entry(t).or_default() += 1;
				}
			}
		}
		let mut out: Vec<(String, usize)> = counts.into_iter().collect();
		out.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
		out.truncate(limit);
		Ok(out)
	}

	/// Saves a search under a name, with the sites and ratings it runs with.
	/// Saving under a name in use replaces that one.
	pub fn save_search(&self, name: &str, query: &str, sites: &[String], ratings: &[Rating]) -> Result<i64> {
		let name = name.trim();
		if name.is_empty() {
			return Err(Error::Query("Give the search a name.".into()));
		}
		parse(query)?;
		let ratings: String = ratings.iter().map(|r| r.letter()).collect();
		self.conn.execute(
			"INSERT INTO saved_searches (name, query, sites, ratings, created_at) VALUES (?, ?, ?, ?, unixepoch())
			ON CONFLICT (name) DO UPDATE SET query = excluded.query, sites = excluded.sites, ratings = excluded.ratings",
			params![name, query.trim(), sites.join(","), ratings],
		)?;
		Ok(self.conn.query_row("SELECT id FROM saved_searches WHERE name = ?", [name], |r| r.get(0))?)
	}

	/// Your saved searches, most recently run first.
	pub fn saved_searches(&self) -> Result<Vec<SavedSearch>> {
		let mut st = self.conn.prepare_cached(
			"SELECT id, name, query, sites, ratings, seen, run_at FROM saved_searches ORDER BY COALESCE(run_at, created_at) DESC, id DESC",
		)?;
		let rows = st.query_map([], |r| {
			let sites: String = r.get(3)?;
			let ratings: String = r.get(4)?;
			let seen: String = r.get(5)?;
			Ok(SavedSearch {
				id: r.get(0)?,
				name: r.get(1)?,
				query: r.get(2)?,
				sites: sites.split(',').filter(|s| !s.is_empty()).map(String::from).collect(),
				ratings: ratings.chars().filter_map(|c| Rating::from_name(&c.to_string())).collect(),
				seen: serde_json::from_str(&seen).unwrap_or_default(),
				run_at: r.get(6)?,
			})
		})?;
		Ok(rows.collect::<rusqlite::Result<_>>()?)
	}

	/// Deletes a saved search, and its query from your history too. With
	/// secure delete on, the rows are overwritten, not just unlinked.
	pub fn delete_search(&self, id: i64) -> Result<()> {
		let query: Option<String> = self.conn.query_row("SELECT query FROM saved_searches WHERE id = ?", [id], |r| r.get(0)).optional()?;
		self.conn.execute("DELETE FROM saved_searches WHERE id = ?", [id])?;
		if let Some(q) = query {
			self.conn.execute("DELETE FROM history WHERE query = ?", [q])?;
		}
		Ok(())
	}

	/// Notes the newest post seen on each site for a saved search, so the next
	/// run can tell what's new. Only ever moves forward.
	pub fn mark_seen(&self, id: i64, newest: &HashMap<String, u64>) -> Result<()> {
		let Some(saved) = self.saved_searches()?.into_iter().find(|s| s.id == id) else { return Ok(()) };
		let mut seen = saved.seen;
		for (site, n) in newest {
			let e = seen.entry(site.clone()).or_insert(0);
			*e = (*e).max(*n);
		}
		self.conn.execute(
			"UPDATE saved_searches SET seen = ?, run_at = unixepoch() WHERE id = ?",
			params![serde_json::to_string(&seen).expect("a map serializes"), id],
		)?;
		Ok(())
	}

	pub fn clear_history(&self) -> Result<()> {
		self.conn.execute("DELETE FROM history", [])?;
		Ok(())
	}

	/// Drops cached posts beyond the newest few thousand, keeping any with a
	/// link or a tag of your own, so the database stays small enough to
	/// encrypt and write out often.
	pub fn prune_posts(&self) -> Result<usize> {
		let sql = self.graph_sql(
			"DELETE FROM posts WHERE key NOT IN (SELECT key FROM posts ORDER BY seen_at DESC, id DESC LIMIT ?)
			AND key NOT IN (SELECT item FROM {links})
			AND key NOT IN (SELECT item FROM {item_tags})
			AND key NOT IN (SELECT key FROM favorites)",
		);
		Ok(self.conn.execute(&sql, [KEEP_POSTS as i64])?)
	}

	/// Fills in, on each post, the links you've made before whose tags are all
	/// on it. They're voted for by `AUTO`, not you, so the window can show them
	/// as filled in and you can keep or turn them down. Returns how many were added.
	pub fn auto_link(&self, posts: &[Post]) -> Result<usize> {
		if !self.auto_links || posts.is_empty() {
			return Ok(0);
		}
		let g = self.graph();
		let mut by_site: HashMap<&str, Vec<LinkSuggestion>> = HashMap::new();
		let mut added = 0;
		for p in posts {
			if !by_site.contains_key(p.site.as_str()) {
				by_site.insert(&p.site, self.site_patterns(&p.site)?);
			}
			let patterns = &by_site[p.site.as_str()];
			if patterns.is_empty() {
				continue;
			}
			let key = p.key();
			for s in g.suggest_links(&key, &p.tags, patterns, &self.actor)? {
				g.add_link(&key, &s.kind, &s.members, AUTO, &p.tags)?;
				added += 1;
			}
		}
		Ok(added)
	}

	/// Links made before that fit this post and it doesn't have yet.
	pub fn suggest_links(&self, post: &Post) -> Result<Vec<LinkSuggestion>> {
		let patterns = self.site_patterns(&post.site)?;
		self.graph().suggest_links(&post.key(), &post.tags, &patterns, &self.actor)
	}

	pub fn cached_alias(&self, site: &str, name: &str) -> Option<Option<String>> {
		self.aliases.get(&(site.to_string(), name.to_string())).cloned()
	}

	pub fn remember_alias(&mut self, site: &str, name: &str, to: Option<String>) {
		self.aliases.insert((site.to_string(), name.to_string()), to);
	}

	/// What one site calls each translated tag: tag → search. An empty search
	/// means the site has nothing like it.
	pub fn translations(&self, site: &str) -> Result<HashMap<String, Expr>> {
		let mut st = self.conn.prepare_cached("SELECT tag, target FROM translations WHERE site = ?")?;
		let rows = st.query_map([site], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
		let mut out = HashMap::new();
		for row in rows {
			let (tag, target) = row?;
			// Targets are checked when saved, so a bad one only comes from editing the file by hand.
			if let Ok(e) = parse(&target) {
				out.insert(tag, e);
			}
		}
		Ok(out)
	}

	/// Every translation, sorted by tag then site.
	pub fn all_translations(&self) -> Result<Vec<Translation>> {
		let mut st = self.conn.prepare_cached("SELECT tag, site, target FROM translations ORDER BY tag, site")?;
		let rows = st.query_map([], |r| Ok(Translation { tag: r.get(0)?, site: r.get(1)?, target: r.get(2)? }))?;
		Ok(rows.collect::<rusqlite::Result<_>>()?)
	}

	/// Says that on `site`, `tag` should be searched as `target` (a search of its
	/// own, like `1girl | 2girls`, or "" for "not on this site").
	pub fn set_translation(&self, site: &str, tag: &str, target: &str) -> Result<()> {
		let tag = tag_name(tag);
		if tag.is_empty() || tag.contains(char::is_whitespace) || tag.starts_with(['-', '~', '(']) {
			return Err(Error::Query("Translate one plain tag at a time.".into()));
		}
		let e = parse(target)?;
		if e.any(&|e| matches!(e, Expr::Link { .. })) {
			return Err(Error::Query("A translation can't hold links.".into()));
		}
		self.conn.execute(
			"INSERT INTO translations (site, tag, target) VALUES (?, ?, ?)
			ON CONFLICT (site, tag) DO UPDATE SET target = excluded.target",
			params![site, tag, target.trim()],
		)?;
		Ok(())
	}

	pub fn remove_translation(&self, site: &str, tag: &str) -> Result<()> {
		self.conn.execute("DELETE FROM translations WHERE site = ? AND tag = ?", params![site, tag_name(tag)])?;
		Ok(())
	}

	/// Your groups and their members, by name: ("animal", ["dog", "rabbit"]).
	pub fn groups(&self) -> Result<Vec<(String, Vec<String>)>> {
		let mut out: Vec<(String, Vec<String>)> = self
			.graph()
			.visible_tags(None)?
			.into_iter()
			.filter_map(|(item, mut tags)| {
				tags.sort();
				item.strip_prefix(GROUP).map(|g| (g.to_string(), tags))
			})
			.collect();
		out.sort();
		Ok(out)
	}

	pub fn group(&self, name: &str) -> Result<Vec<String>> {
		let name = tag_name(name.trim_start_matches('@'));
		let mut tags = self.graph().visible_tags(Some(&format!("{GROUP}{name}")))?.into_values().next().unwrap_or_default();
		tags.sort();
		Ok(tags)
	}

	/// Puts a tag in a group (making the group if it's new) or takes it out.
	pub fn set_in_group(&self, group: &str, tag: &str, on: bool) -> Result<()> {
		let group = tag_name(group.trim_start_matches('@'));
		if group.is_empty() || !group.chars().all(|c| c.is_alphanumeric() || c == '_') {
			return Err(Error::Query("Group names are letters, digits and _.".into()));
		}
		self.graph().vote_tag(&format!("{GROUP}{group}"), tag, &self.actor, if on { 1 } else { 0 })
	}

	/// What the relation index says the subject does the verb with: learned
	/// ones, ones you kept, and your own relations (links on `RULES`), best
	/// first. Rejected ones only with `rejected`.
	pub fn relations(&self, subject: &str, verb: &str, rejected: bool) -> Result<Vec<RelationEntry>> {
		let (subject, verb) = (tag_name(subject), tag_name(verb));
		let mut st = self.conn.prepare_cached(
			"SELECT object, score, state FROM relation_index WHERE subject = ? AND verb = ? ORDER BY state = 'kept' DESC, score DESC",
		)?;
		let rows = st.query_map(params![subject, verb], |r| Ok((r.get::<_, String>(0)?, r.get::<_, f64>(1)?, r.get::<_, String>(2)?)))?;
		let mut out: Vec<RelationEntry> = Vec::new();
		for row in rows {
			let (object, score, state) = row?;
			let state = RelationState::from_name(&state);
			if rejected || state != RelationState::Rejected {
				out.push(RelationEntry { subject: subject.clone(), verb: verb.clone(), object, score: score as f32, state });
			}
		}
		for r in self.rules()? {
			if r.kind == verb && r.members.contains(&subject) {
				for m in r.members.into_iter().filter(|m| *m != subject) {
					if !out.iter().any(|e| e.object == m) {
						out.insert(0, RelationEntry { subject: subject.clone(), verb: verb.clone(), object: m, score: f32::MAX, state: RelationState::Kept });
					}
				}
			}
		}
		Ok(out)
	}

	/// Whether relations for this subject and verb were learned already (even if none were found).
	pub fn has_learned(&self, subject: &str, verb: &str) -> Result<bool> {
		Ok(self
			.conn
			.query_row("SELECT 1 FROM relation_learned WHERE subject = ? AND verb = ?", params![tag_name(subject), tag_name(verb)], |_| Ok(()))
			.optional()?
			.is_some())
	}

	/// Stores what was learned. Ones you kept or rejected stay that way.
	pub fn remember_learned(&self, subject: &str, verb: &str, site: &str, learned: &[crate::learn::Learned]) -> Result<()> {
		let (subject, verb) = (tag_name(subject), tag_name(verb));
		let tx = self.conn.unchecked_transaction()?;
		for l in learned {
			tx.execute(
				"INSERT INTO relation_index (subject, verb, object, score, state, site, at) VALUES (?, ?, ?, ?, 'learned', ?, unixepoch())
				ON CONFLICT (subject, verb, object) DO UPDATE SET score = excluded.score, at = excluded.at WHERE state = 'learned'",
				params![subject, verb, l.object, l.score as f64, site],
			)?;
		}
		tx.execute(
			"INSERT INTO relation_learned (subject, verb, site, at) VALUES (?, ?, ?, unixepoch())
			ON CONFLICT (subject, verb) DO UPDATE SET site = excluded.site, at = excluded.at",
			params![subject, verb, site],
		)?;
		tx.commit()?;
		Ok(())
	}

	/// Keeps, rejects or puts back (learned) one relation in the index.
	pub fn set_relation(&self, subject: &str, verb: &str, object: &str, state: RelationState) -> Result<()> {
		self.conn.execute(
			"INSERT INTO relation_index (subject, verb, object, score, state, site, at) VALUES (?, ?, ?, 0, ?, '', unixepoch())
			ON CONFLICT (subject, verb, object) DO UPDATE SET state = excluded.state, at = excluded.at",
			params![tag_name(subject), tag_name(verb), tag_name(object), state.name()],
		)?;
		Ok(())
	}

	/// Forgets what was learned for a subject and verb (keeping your choices), so it's learned again.
	pub fn forget_learned(&self, subject: &str, verb: &str) -> Result<()> {
		let (subject, verb) = (tag_name(subject), tag_name(verb));
		self.conn.execute("DELETE FROM relation_index WHERE subject = ? AND verb = ? AND state = 'learned'", params![subject, verb])?;
		self.conn.execute("DELETE FROM relation_learned WHERE subject = ? AND verb = ?", params![subject, verb])?;
		Ok(())
	}

	/// Every subject and verb in the index, for the Groups & relations window.
	pub fn relation_topics(&self) -> Result<Vec<(String, String)>> {
		let mut st = self.conn.prepare_cached("SELECT DISTINCT subject, verb FROM relation_index ORDER BY verb, subject")?;
		let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
		Ok(rows.collect::<rusqlite::Result<_>>()?)
	}

	/// Relations you've set between tags, as links on `RULES`.
	pub fn rules(&self) -> Result<Vec<ItemLink>> {
		Ok(self.graph().item_links(RULES, &self.actor)?.into_iter().filter(|l| l.score > 0).collect())
	}

	/// Says `members` relate as `kind` wherever they're together, e.g. eat: rabbit carrot.
	/// Posts with all of them get the link filled in.
	pub fn add_rule(&self, kind: &str, members: &[String]) -> Result<()> {
		self.graph().add_link(RULES, kind, members, &self.actor, members).map(|_| ())
	}

	/// Relation names: every link name but the first, which means "any link".
	pub fn relation_names(&self) -> Result<Vec<String>> {
		let mut names = self.graph().link_names()?;
		if !names.is_empty() {
			names.remove(0);
		}
		Ok(names)
	}

	/// Finds relations asked about with `relation` (or `link`, or an `@word`
	/// that isn't one of your groups): `rabbit eat relation` and
	/// `rabbit relation eat` ask what rabbits eat. The word is taken out and
	/// `asked` says who and which verb; the search then uses the relation index.
	/// Without it, words are plain tags: `rabbit eat` is a rabbit eating anything.
	pub fn read_relations(&self, expr: &Expr) -> Result<Reading> {
		let groups: Vec<String> = self.groups()?.into_iter().map(|(g, _)| format!("@{g}")).collect();
		let is_group = |t: &str| groups.iter().any(|g| g == t);
		let words = Words { is_group: &is_group, generic: &self.opts.link_names };
		let mut asked = Vec::new();
		let expr = ask(expr, &words, &mut asked)?;
		Ok(Reading { expr, asked })
	}

	/// Spells out what a link stands for, from the relations and links you've
	/// made: eat[rabbit] becomes eat[carrot rabbit] | eat[lettuce rabbit].
	/// Combinations nothing relates are dropped, so eat[@animal @object] only
	/// asks the site about pairs you've related. An error if none are left.
	pub fn expand_relations(&self, expr: &Expr) -> Result<Expr> {
		let g = self.graph();
		let any = g.any_link().to_string();
		let patterns = g.link_patterns(&self.actor)?;
		fn go(e: &Expr, patterns: &[LinkSuggestion], any: &str) -> Option<Expr> {
			match e {
				Expr::Link { kind, tags } => {
					let mut found: Vec<Expr> = Vec::new();
					for p in patterns {
						let kind_fits = kind.as_deref().is_none_or(|k| k == any || k == p.kind);
						if kind_fits && tags.iter().all(|t| p.members.contains(t)) {
							let link = Expr::Link { kind: Some(p.kind.clone()), tags: p.members.clone() };
							if !found.contains(&link) {
								found.push(link);
							}
						}
					}
					match found.len() {
						0 => None,
						1 => found.pop(),
						_ => Some(Expr::Or(found)),
					}
				}
				Expr::Not(inner) => Some(go(inner, patterns, any).map_or(Expr::And(Vec::new()), |i| Expr::Not(Box::new(i)))),
				Expr::And(es) => Some(Expr::And(es.iter().map(|e| go(e, patterns, any)).collect::<Option<_>>()?)),
				Expr::Or(es) => {
					let sides: Vec<Expr> = es.iter().filter_map(|e| go(e, patterns, any)).collect();
					(!sides.is_empty()).then_some(Expr::Or(sides))
				}
				e => Some(e.clone()),
			}
		}
		go(expr, &patterns, &any).ok_or_else(|| {
			Error::Query("Nothing you've related fits this link yet. Add a relation in Groups & relations, or link the tags on a post.".into())
		})
	}

	/// The links worth filling in on one site's posts: your patterns, with each
	/// tag swapped for what that site calls it (a one-tag translation, then the
	/// site's alias), so dog —holding→ guitar also fits posts tagged domestic_dog.
	fn site_patterns(&self, site: &str) -> Result<Vec<LinkSuggestion>> {
		let translations = self.translations(site)?;
		let mut out: Vec<LinkSuggestion> = Vec::new();
		for mut p in self.graph().link_patterns(&self.actor)? {
			for m in &mut p.members {
				if let Some(Expr::Tag(t)) = translations.get(m.as_str()) {
					*m = t.clone();
				}
				if let Some(Some(to)) = self.cached_alias(site, m) {
					*m = to;
				}
			}
			p.members.sort();
			p.members.dedup();
			if p.members.len() >= 2 && !out.iter().any(|q| q.kind == p.kind && q.members == p.members) {
				out.push(p);
			}
		}
		Ok(out)
	}

	/// Every tag in your links and relations, so a search can look up what each
	/// site calls them.
	pub fn pattern_tags(&self) -> Result<Vec<String>> {
		let mut out: Vec<String> = self.graph().link_patterns(&self.actor)?.into_iter().flat_map(|p| p.members).collect();
		out.sort();
		out.dedup();
		Ok(out)
	}

	/// Spells out `@group`s: `@animal` alone is any of its tags, and
	/// `eat[@animal @object]` is any eat link between one of each.
	pub fn expand_groups(&self, expr: &Expr) -> Result<Expr> {
		let members = |t: &str| -> Result<Vec<String>> {
			let m = self.group(t)?;
			if m.is_empty() {
				return Err(Error::Query(format!(
					"There's no group “{t}” yet. Make one in Groups & relations. (Beside a relation, like “dog eat {t}”, it means anything.)"
				)));
			}
			Ok(m)
		};
		Ok(match expr {
			Expr::Tag(t) if t.starts_with('@') => {
				let m = members(t)?;
				if m.len() == 1 { Expr::Tag(m[0].clone()) } else { Expr::Or(m.into_iter().map(Expr::Tag).collect()) }
			}
			Expr::Link { kind, tags } if tags.iter().any(|t| t.starts_with('@')) => {
				let mut combos: Vec<Vec<String>> = vec![Vec::new()];
				for t in tags {
					let options = if t.starts_with('@') { members(t)? } else { vec![t.clone()] };
					let mut next = Vec::new();
					for c in &combos {
						for o in options.iter().filter(|o| !c.contains(o)) {
							let mut c = c.clone();
							c.push(o.clone());
							next.push(c);
						}
					}
					combos = next;
					if combos.len() > MAX_COMBOS {
						return Err(Error::Query(format!("That stands for more than {MAX_COMBOS} links; use smaller groups.")));
					}
				}
				let mut links: Vec<Expr> = Vec::new();
				for mut c in combos.into_iter().filter(|c| c.len() == tags.len()) {
					c.sort();
					let link = Expr::Link { kind: kind.clone(), tags: c };
					if !links.contains(&link) {
						links.push(link);
					}
				}
				match links.len() {
					0 => return Err(Error::Query("Those groups don't leave two different tags to link.".into())),
					1 => links.pop().unwrap(),
					_ => Expr::Or(links),
				}
			}
			Expr::Not(e) => Expr::Not(Box::new(self.expand_groups(e)?)),
			Expr::And(es) => Expr::And(es.iter().map(|e| self.expand_groups(e)).collect::<Result<_>>()?),
			Expr::Or(es) => Expr::Or(es.iter().map(|e| self.expand_groups(e)).collect::<Result<_>>()?),
			e => e.clone(),
		})
	}

	/// Writes link tags the way the graph stores them now, following renames and merges.
	pub fn resolve_links(&self, expr: &Expr) -> Result<Expr> {
		let g = self.graph();
		Ok(match expr {
			Expr::Link { kind, tags } => {
				let kind = match kind {
					Some(k) if k != g.any_link() => Some(g.canonical(k)?.unwrap_or_else(|| k.clone())),
					_ => None,
				};
				let mut out = Vec::new();
				for t in tags {
					out.push(g.canonical(t)?.unwrap_or_else(|| g.normalize(t)));
				}
				Expr::Link { kind, tags: out }
			}
			Expr::Not(e) => Expr::Not(Box::new(self.resolve_links(e)?)),
			Expr::And(es) => Expr::And(es.iter().map(|e| self.resolve_links(e)).collect::<Result<_>>()?),
			Expr::Or(es) => Expr::Or(es.iter().map(|e| self.resolve_links(e)).collect::<Result<_>>()?),
			e => e.clone(),
		})
	}

	/// A post with this computer's tags and links on it, ready to check against a search.
	pub fn subject<'p>(&self, post: &'p Post) -> Result<PostSubject<'p>> {
		let key = post.key();
		let g = self.graph();
		let extra = g.visible_tags(Some(&key))?.remove(&key).unwrap_or_default();
		let links = g.item_links(&key, &self.actor)?;
		let favorite = self.is_favorite(&key)?;
		Ok(PostSubject { post, extra, links, favorite })
	}
}

/// `read_relations` on one expression: in each run of ANDed words, a relation
/// name takes the plain tags right before and after it.
/// What `ask` needs to know about your groups and link names.
struct Words<'a> {
	is_group: &'a dyn Fn(&str) -> bool,
	/// The general link names ("link", "relation").
	generic: &'a [String],
}

impl Words<'_> {
	/// A word that stands for anything beside a relation: "link", "relation",
	/// or an `@word` that isn't one of your groups.
	fn is_anything(&self, t: &str) -> bool {
		self.generic.iter().any(|g| g == t) || (t.starts_with('@') && !(self.is_group)(t))
	}
}

/// A relation asked about with `@relation`: who (if anyone) and the verb, in your words.
#[derive(Debug, Clone, PartialEq)]
pub struct Asked {
	pub subject: Option<String>,
	pub verb: String,
}

/// What `read_relations` made of a search.
#[derive(Debug, Clone, PartialEq)]
pub struct Reading {
	pub expr: Expr,
	pub asked: Vec<Asked>,
}

/// Finds `@relation`-style asks in runs of ANDed words and takes the
/// placeholder out: the subject is the first plain word of the run, the verb
/// the word next to the placeholder (the one before it if both are plain).
fn ask(e: &Expr, w: &Words, asked: &mut Vec<Asked>) -> Result<Expr> {
	let plain = |e: &Expr| matches!(e, Expr::Tag(t) if !w.is_anything(t) && !t.contains('*'));
	Ok(match e {
		Expr::Tag(t) if t.starts_with('@') && w.is_anything(t) => {
			return Err(Error::Query(format!("“{t}” on its own doesn't ask anything. Write who and what, like “dog eat {t}”.")));
		}
		Expr::And(es) => {
			// "link" and "relation" are tags too (Link is a character on e621), so
			// they only ask with a subject and a verb around them; an @word always asks.
			let others = es.iter().filter(|e| plain(e)).count();
			let asks = |e: &Expr| matches!(e, Expr::Tag(t) if w.is_anything(t) && (t.starts_with('@') || others >= 2));
			let Some(p) = es.iter().position(asks) else {
				return Ok(Expr::And(es.iter().map(|e| ask(e, w, asked)).collect::<Result<_>>()?));
			};
			let subject = es.iter().position(plain);
			let near = |i: Option<usize>| i.filter(|&i| i < es.len() && plain(&es[i]) && Some(i) != subject);
			let verb = near(p.checked_sub(1)).or(near(Some(p + 1))).or(subject);
			let Some(v) = verb else {
				let Expr::Tag(t) = &es[p] else { unreachable!() };
				return Err(Error::Query(format!("Write who and what to ask about, like “dog eat {t}”.")));
			};
			let tag = |i: usize| match &es[i] {
				Expr::Tag(t) => t.clone(),
				_ => unreachable!(),
			};
			asked.push(Asked { subject: subject.filter(|&s| s != v).map(tag), verb: tag(v) });
			let rest: Vec<Expr> = es.iter().enumerate().filter(|(i, _)| *i != p).map(|(_, e)| e.clone()).collect();
			let rest: Vec<Expr> = rest.iter().map(|e| ask(e, w, asked)).collect::<Result<_>>()?;
			if rest.len() == 1 { rest.into_iter().next().unwrap() } else { Expr::And(rest) }
		}
		Expr::Or(es) => Expr::Or(es.iter().map(|e| ask(e, w, asked)).collect::<Result<_>>()?),
		Expr::Not(inner) => Expr::Not(Box::new(ask(inner, w, asked)?)),
		e => e.clone(),
	})
}

/// Tags as searches write them: lowercase, `_` for spaces.
fn tag_name(raw: &str) -> String {
	raw.trim().to_lowercase().split_whitespace().collect::<Vec<_>>().join("_")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RelationState {
	/// Found by the learner; used until you say otherwise.
	Learned,
	Kept,
	/// Never used, and not learned again.
	Rejected,
}

impl RelationState {
	fn name(self) -> &'static str {
		match self {
			RelationState::Learned => "learned",
			RelationState::Kept => "kept",
			RelationState::Rejected => "rejected",
		}
	}

	fn from_name(s: &str) -> Self {
		match s {
			"kept" => RelationState::Kept,
			"rejected" => RelationState::Rejected,
			_ => RelationState::Learned,
		}
	}
}

/// One entry of the relation index: the subject does the verb with the object.
#[derive(Debug, Clone, PartialEq)]
pub struct RelationEntry {
	pub subject: String,
	pub verb: String,
	pub object: String,
	pub score: f32,
	pub state: RelationState,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SavedSearch {
	pub id: i64,
	pub name: String,
	pub query: String,
	pub sites: Vec<String>,
	pub ratings: Vec<Rating>,
	/// Newest post id seen on each site the last time it ran; newer ones are new.
	pub seen: HashMap<String, u64>,
	/// When it last ran (Unix seconds).
	pub run_at: Option<i64>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Translation {
	pub tag: String,
	pub site: String,
	pub target: String,
}

pub struct PostSubject<'p> {
	pub post: &'p Post,
	/// Tags added here, on top of the site's.
	pub extra: Vec<String>,
	pub links: Vec<ItemLink>,
	/// In your favorites.
	pub favorite: bool,
}

impl Subject for PostSubject<'_> {
	fn has_tag(&self, tag: &str) -> bool {
		self.post.has_tag(tag) || self.extra.iter().any(|t| t == tag)
	}

	fn meta(&self, key: &str, value: &str) -> Option<bool> {
		let p = self.post;
		match key {
			"rating" => Rating::from_name(value).map(|r| r == p.rating),
			"score" => compare(p.score, value),
			"id" => compare(p.id as i64, value),
			"favcount" => compare(p.fav_count as i64, value),
			"width" => compare(p.width as i64, value),
			"height" => compare(p.height as i64, value),
			"type" => Some(p.ext.eq_ignore_ascii_case(value)),
			"md5" => p.md5.as_ref().map(|m| m.eq_ignore_ascii_case(value)),
			"order" => Some(true),
			"favorited" => match value {
				"yes" | "true" | "1" => Some(self.favorite),
				"no" | "false" | "0" => Some(!self.favorite),
				_ => None,
			},
			_ => None,
		}
	}

	fn linked(&self, kind: Option<&str>, tags: &[String]) -> bool {
		self.links.iter().any(|l| l.score > 0 && kind.is_none_or(|k| k == l.kind) && tags.iter().all(|t| l.members.contains(t)))
	}

	fn tags(&self) -> Box<dyn Iterator<Item = &str> + '_> {
		Box::new(self.post.tags.iter().chain(&self.extra).map(String::as_str))
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn round_trips_through_bytes() {
		let mut s = Store::in_memory().unwrap();
		s.set_value("settings", "{}").unwrap();
		s.add_history("fox (smile | grin)", &["e926".into()]).unwrap();
		s.add_history("wolf", &[]).unwrap();
		s.add_history("fox (smile | grin)", &[]).unwrap();
		let bytes = s.to_bytes().unwrap();
		let back = Store::from_bytes(&bytes).unwrap();
		assert_eq!(back.value("settings").unwrap().as_deref(), Some("{}"));
		let h: Vec<String> = back.history(10).unwrap().into_iter().map(|(q, _)| q).collect();
		assert_eq!(h, vec!["fox (smile | grin)", "wolf"]);
		assert_eq!(back.history_tags(2).unwrap(), vec![("fox".into(), 1), ("grin".into(), 1)]);
		assert!(back.graph().link_names().unwrap().contains(&"relation".to_string()));
	}

	#[test]
	fn groups_and_relations() {
		let s = Store::in_memory().unwrap();
		for (g, t) in [("animal", "rabbit"), ("animal", "dog"), ("object", "carrot"), ("object", "bone")] {
			s.set_in_group(g, t, true).unwrap();
		}
		assert_eq!(s.groups().unwrap(), vec![("animal".into(), vec!["dog".into(), "rabbit".into()]), ("object".into(), vec!["bone".into(), "carrot".into()])]);
		let e = s.expand_groups(&parse("eat[@animal @object] @animal").unwrap()).unwrap();
		let link = |a: &str, b: &str| Expr::Link { kind: Some("eat".into()), tags: vec![a.into(), b.into()] };
		let either = Expr::Or(vec![Expr::Tag("dog".into()), Expr::Tag("rabbit".into())]);
		let links = Expr::Or(vec![link("bone", "dog"), link("carrot", "dog"), link("bone", "rabbit"), link("carrot", "rabbit")]);
		assert_eq!(e, Expr::And(vec![links, either]));
		assert!(s.expand_groups(&parse("@plant").unwrap()).is_err());

		// A relation set once fills in on any post with both tags.
		s.graph().add_link_name("eat", &s.actor).unwrap();
		s.add_rule("eat", &["rabbit".into(), "carrot".into()]).unwrap();
		assert_eq!(s.rules().unwrap().len(), 1);
		let post = |id: u64, tags: &[&str]| Post {
			site: "test".into(),
			id,
			tags: tags.iter().map(|t| t.to_string()).collect(),
			categories: Default::default(),
			rating: Rating::Safe,
			score: 0,
			fav_count: 0,
			width: 1,
			height: 1,
			ext: "png".into(),
			md5: None,
			file_url: None,
			sample_url: None,
			preview_url: None,
			page_url: String::new(),
			sources: Vec::new(),
			created_at: String::new(),
		};
		let posts = [post(1, &["carrot", "field", "rabbit"]), post(2, &["bone", "rabbit"])];
		assert_eq!(s.auto_link(&posts).unwrap(), 1);
		let filter = s.resolve_links(&e).unwrap();
		assert!(filter.matches(&s.subject(&posts[0]).unwrap()));
		assert!(!filter.matches(&s.subject(&posts[1]).unwrap()));
	}


	#[test]
	fn reads_relations_in_words() {
		let s = Store::in_memory().unwrap();
		s.graph().add_link_name("eat", &s.actor).unwrap();
		let read = |q: &str| s.read_relations(&parse(q).unwrap()).unwrap().expr;
		let link = |tags: &[&str]| Expr::Link { kind: Some("eat".into()), tags: tags.iter().map(|t| t.to_string()).collect() };
		// Without "relation", words are plain tags, even relation names: a rabbit eating anything.
		assert_eq!(read("rabbit eat carrot"), parse("rabbit eat carrot").unwrap());
		assert_eq!(read("rabbit eat"), parse("rabbit eat").unwrap());
		// Brackets still mean a link.
		assert_eq!(read("eat[rabbit carrot]"), link(&["rabbit", "carrot"]));
		// @relation (or link, relation, an @word that isn't a group) asks: any verb,
		// nothing set up. The placeholder goes; the verb stays a tag to search.
		let asked = |q: &str| {
			let r = s.read_relations(&parse(q).unwrap()).unwrap();
			(r.expr, r.asked.into_iter().map(|a| (a.subject, a.verb)).collect::<Vec<_>>())
		};
		let dog_chew = (parse("dog chew").unwrap(), vec![(Some("dog".to_string()), "chew".to_string())]);
		for q in ["dog chew @relation", "dog @relation chew", "dog chew link", "dog chew relation", "dog chew @what"] {
			assert_eq!(asked(q), dog_chew, "{q}");
		}
		assert_eq!(asked("rabbit eat relation"), (parse("rabbit eat").unwrap(), vec![(Some("rabbit".to_string()), "eat".to_string())]));
		// Other words stay as they are; the verb is the word by the placeholder.
		assert_eq!(asked("dog eat @relation solo"), (parse("dog eat solo").unwrap(), vec![(Some("dog".to_string()), "eat".to_string())]));
		assert_eq!(asked("eat @relation"), (parse("eat").unwrap(), vec![(None, "eat".to_string())]));
		assert!(s.read_relations(&parse("@relation").unwrap()).is_err());
		// A real group is still a group, not a question.
		s.set_in_group("food", "carrot", true).unwrap();
		assert_eq!(asked("rabbit eat @food").1, Vec::new());
		// "link" stays a plain tag elsewhere (it's a character on e621).
		assert_eq!(read("link zelda"), parse("link zelda").unwrap());
		assert_eq!(read("hylian link"), parse("hylian link").unwrap());
	}

	#[test]
	fn expands_links_from_relations() {
		let s = Store::in_memory().unwrap();
		s.graph().add_link_name("eat", &s.actor).unwrap();
		for (a, b) in [("rabbit", "carrot"), ("rabbit", "lettuce"), ("dog", "bone")] {
			s.add_rule("eat", &[a.into(), b.into()]).unwrap();
		}
		let expand = |q: &str| s.expand_relations(&parse(q).unwrap());
		let link = |a: &str, b: &str| Expr::Link { kind: Some("eat".into()), tags: vec![a.into(), b.into()] };
		assert_eq!(expand("eat[rabbit]").unwrap(), Expr::Or(vec![link("carrot", "rabbit"), link("lettuce", "rabbit")]));
		// Pairs nothing relates drop out.
		assert_eq!(expand("eat[rabbit bone] | eat[dog bone]").unwrap(), Expr::Or(vec![link("bone", "dog")]));
		// Any link name, and leaving one out.
		assert_eq!(expand("[bone]").unwrap(), link("bone", "dog"));
		assert_eq!(expand("solo -eat[dog]").unwrap(), parse("solo -eat[bone dog]").unwrap());
		assert!(expand("eat[fox]").is_err());
		assert!(expand("relation[rabbit]").is_err());
	}


	#[test]
	fn saved_searches_save_track_and_delete() {
		let mut s = Store::in_memory().unwrap();
		let id = s.save_search("rabbits", "rabbit eat", &["e926".into(), "danbooru".into()], &[Rating::Safe]).unwrap();
		s.add_history("rabbit eat", &[]).unwrap();
		assert!(s.save_search("", "x", &[], &[]).is_err());
		assert!(s.save_search("bad", "a ]", &[], &[]).is_err());
		// Saving under the same name replaces it.
		assert_eq!(s.save_search("rabbits", "rabbit eat solo", &["e926".into()], &[Rating::Safe, Rating::Questionable]).unwrap(), id);
		let all = s.saved_searches().unwrap();
		assert_eq!(all.len(), 1);
		assert_eq!((all[0].query.as_str(), all[0].sites.clone(), all[0].ratings.clone()), ("rabbit eat solo", vec!["e926".to_string()], vec![Rating::Safe, Rating::Questionable]));

		// Seen only moves forward.
		s.mark_seen(id, &HashMap::from([("e926".into(), 50)])).unwrap();
		s.mark_seen(id, &HashMap::from([("e926".into(), 40), ("danbooru".into(), 7)])).unwrap();
		assert_eq!(s.saved_searches().unwrap()[0].seen, HashMap::from([("e926".into(), 50), ("danbooru".into(), 7)]));

		// Deleting takes it and its history entry, and leaves no trace in the bytes.
		s.save_search("keep", "fox", &[], &[Rating::Safe]).unwrap();
		s.add_history("rabbit eat solo", &[]).unwrap();
		s.delete_search(id).unwrap();
		assert_eq!(s.saved_searches().unwrap().iter().map(|x| x.name.as_str()).collect::<Vec<_>>(), vec!["keep"]);
		assert!(s.history(10).unwrap().iter().all(|(q, _)| q != "rabbit eat solo"));
		let bytes = s.to_bytes().unwrap();
		assert!(!bytes.windows(15).any(|w| w == b"rabbit eat solo"), "deleted search still in the database bytes");
		assert!(bytes.windows(4).any(|w| w == b"keep"));
	}

}
