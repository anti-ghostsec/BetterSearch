// Personal tags and links between them, kept in SQLite. A Rust port of the
// site's taggraph (graph.ts), for one person on one machine: same schema and
// ideas, without the rate limits, blocking and adult flags a public site needs.
// Votes still record who made them, so a shared graph can sync later.
//
// Items are opaque keys ("e621:post/991"): the graph never knows what they
// are. Tags go on items and are voted up or down. A link is a named group of
// tags that are already on one item, e.g. a "relation" link of rabbit · eat ·
// carrot, so a search for `[eat]` finds every item where eat is linked to
// something. Link names are tags too, of kind "link".

mod names;

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::{Connection, OptionalExtension, Row, params, params_from_iter, types::Value};

pub use names::{MAX_NAME, name_error, namespace_of, normalize_name};

use crate::Result;

/// A refusal to show the person, e.g. a bad tag name.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
#[error("{0}")]
pub struct TagGraphError(pub String);

fn refuse<T>(msg: impl Into<String>) -> Result<T> {
	Err(TagGraphError(msg.into()).into())
}

pub type Vote = i8;

#[derive(Debug, Clone, PartialEq)]
pub struct ItemTag {
	pub name: String,
	pub namespace: String,
	pub score: i64,
	pub mine: Vote,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ItemLink {
	pub id: i64,
	pub item: String,
	pub kind: String,
	pub members: Vec<String>,
	pub score: i64,
	pub mine: Vote,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LinkMatch {
	pub kind: String,
	pub members: Vec<String>,
}

/// A link someone made before, offered for an item that has all its tags.
#[derive(Debug, Clone, PartialEq)]
pub struct LinkSuggestion {
	pub kind: String,
	pub members: Vec<String>,
	/// Items that already carry it.
	pub uses: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TagInfo {
	pub id: i64,
	pub name: String,
	pub kind: String,
	pub alias_of: Option<String>,
	/// Items it's on, plus links it's in.
	pub uses: i64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LogEntry {
	pub id: i64,
	pub at: i64,
	pub actor: Option<String>,
	pub action: String,
	pub item: Option<String>,
	pub detail: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stats {
	pub tags: i64,
	pub link_names: i64,
	pub tagged_items: i64,
	pub links: i64,
}

#[derive(Debug, Clone)]
pub struct Options {
	/// Table name prefix, so the graph can share a database.
	pub prefix: String,
	/// Namespaces a tag may start with, e.g. "character:rem".
	pub namespaces: Vec<String>,
	/// Link names created on first run. The first means "any link" in searches.
	pub link_names: Vec<String>,
	/// Most tags one link may join.
	pub max_link_size: usize,
}

impl Default for Options {
	fn default() -> Self {
		Options {
			prefix: "tg".into(),
			namespaces: vec!["character".into(), "meta".into()],
			link_names: vec!["link".into(), "relation".into()],
			max_link_size: 5,
		}
	}
}

#[derive(Debug, Clone)]
struct TagRow {
	id: i64,
	name: String,
	kind: String,
	alias_of: Option<i64>,
}

impl TagRow {
	const COLS: &'static str = "id, name, kind, alias_of";

	fn from(r: &Row) -> rusqlite::Result<Self> {
		Ok(TagRow { id: r.get(0)?, name: r.get(1)?, kind: r.get(2)?, alias_of: r.get(3)? })
	}
}

const SCHEMA: &[&str] = &["
	CREATE TABLE {tags} (
		id INTEGER PRIMARY KEY,
		name TEXT NOT NULL UNIQUE,
		kind TEXT NOT NULL DEFAULT 'tag',
		alias_of INTEGER REFERENCES {tags}(id) ON DELETE SET NULL,
		created_by TEXT,
		created_at INTEGER NOT NULL
	);
	CREATE TABLE {item_tags} (
		item TEXT NOT NULL,
		tag_id INTEGER NOT NULL REFERENCES {tags}(id) ON DELETE CASCADE,
		actor TEXT NOT NULL,
		vote INTEGER NOT NULL,
		at INTEGER NOT NULL,
		PRIMARY KEY (item, tag_id, actor)
	);
	CREATE INDEX {item_tags}_tag ON {item_tags}(tag_id);
	CREATE INDEX {item_tags}_actor ON {item_tags}(actor);
	CREATE TABLE {links} (
		id INTEGER PRIMARY KEY,
		item TEXT NOT NULL,
		kind_id INTEGER NOT NULL REFERENCES {tags}(id) ON DELETE CASCADE,
		key TEXT NOT NULL,
		created_by TEXT,
		created_at INTEGER NOT NULL,
		UNIQUE (item, kind_id, key)
	);
	CREATE INDEX {links}_item ON {links}(item);
	CREATE TABLE {link_members} (
		link_id INTEGER NOT NULL REFERENCES {links}(id) ON DELETE CASCADE,
		pos INTEGER NOT NULL,
		tag_id INTEGER NOT NULL REFERENCES {tags}(id) ON DELETE CASCADE,
		PRIMARY KEY (link_id, pos)
	);
	CREATE INDEX {link_members}_tag ON {link_members}(tag_id);
	CREATE TABLE {link_votes} (
		link_id INTEGER NOT NULL REFERENCES {links}(id) ON DELETE CASCADE,
		actor TEXT NOT NULL,
		vote INTEGER NOT NULL,
		at INTEGER NOT NULL,
		PRIMARY KEY (link_id, actor)
	);
	CREATE INDEX {link_votes}_actor ON {link_votes}(actor);
	CREATE TABLE {log} (
		id INTEGER PRIMARY KEY,
		at INTEGER NOT NULL,
		actor TEXT,
		action TEXT NOT NULL,
		item TEXT,
		detail TEXT NOT NULL
	);
	CREATE INDEX {log}_actor ON {log}(actor, at);
"];

fn now() -> i64 {
	std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64)
}

fn placeholders(n: usize) -> String {
	vec!["?"; n].join(",")
}

/// A view of the graph over a connection. Cheap to make; `Store::graph` hands these out.
pub struct TagGraph<'c> {
	conn: &'c Connection,
	opts: &'c Options,
}

impl<'c> TagGraph<'c> {
	/// Creates or updates the tables. Call once per database before using the graph.
	pub fn migrate(conn: &Connection, opts: &Options) -> Result<()> {
		let g = TagGraph { conn, opts };
		conn.execute_batch(&g.sql("CREATE TABLE IF NOT EXISTS {meta} (key TEXT PRIMARY KEY, value TEXT NOT NULL)"))?;
		let version: usize = conn
			.query_row(&g.sql("SELECT value FROM {meta} WHERE key = 'version'"), [], |r| r.get::<_, String>(0))
			.optional()?
			.and_then(|v| v.parse().ok())
			.unwrap_or(0);
		for (i, step) in SCHEMA.iter().enumerate().skip(version) {
			let tx = conn.unchecked_transaction()?;
			tx.execute_batch(&g.sql(step))?;
			tx.execute(
				&g.sql("INSERT INTO {meta} (key, value) VALUES ('version', ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value"),
				[(i + 1).to_string()],
			)?;
			if i == 0 {
				for n in &opts.link_names {
					tx.execute(&g.sql("INSERT INTO {tags} (name, kind, created_at) VALUES (?, 'link', ?)"), params![n, now()])?;
				}
			}
			tx.commit()?;
		}
		Ok(())
	}

	pub fn new(conn: &'c Connection, opts: &'c Options) -> Self {
		TagGraph { conn, opts }
	}

	/// The search word meaning "linked by anything".
	pub fn any_link(&self) -> &str {
		&self.opts.link_names[0]
	}

	// ── Plumbing ────────────────────────────────────────────────────────────

	fn sql(&self, text: &str) -> String {
		let mut out = String::with_capacity(text.len() + 32);
		let mut rest = text;
		while let Some(open) = rest.find('{') {
			let close = open + rest[open..].find('}').expect("unclosed { in SQL");
			out.push_str(&rest[..open]);
			out.push_str(&self.opts.prefix);
			out.push('_');
			out.push_str(&rest[open + 1..close]);
			rest = &rest[close + 1..];
		}
		out.push_str(rest);
		out
	}

	fn exec(&self, text: &str, args: impl rusqlite::Params) -> Result<usize> {
		Ok(self.conn.prepare_cached(&self.sql(text))?.execute(args)?)
	}

	fn rows<T>(&self, text: &str, args: impl rusqlite::Params, f: impl FnMut(&Row) -> rusqlite::Result<T>) -> Result<Vec<T>> {
		let mut st = self.conn.prepare_cached(&self.sql(text))?;
		let out = st.query_map(args, f)?.collect::<rusqlite::Result<Vec<T>>>()?;
		Ok(out)
	}

	fn row<T>(&self, text: &str, args: impl rusqlite::Params, f: impl FnOnce(&Row) -> rusqlite::Result<T>) -> Result<Option<T>> {
		Ok(self.conn.prepare_cached(&self.sql(text))?.query_row(args, f).optional()?)
	}

	fn count(&self, text: &str) -> Result<i64> {
		Ok(self.row(text, [], |r| r.get(0))?.unwrap_or(0))
	}

	fn log(&self, actor: Option<&str>, action: &str, item: Option<&str>, detail: &str) -> Result<()> {
		self.exec("INSERT INTO {log} (at, actor, action, item, detail) VALUES (?, ?, ?, ?, ?)", params![now(), actor, action, item, detail])?;
		Ok(())
	}

	pub fn normalize(&self, name: &str) -> String {
		normalize_name(name, &self.opts.namespaces)
	}

	fn tag_by_id(&self, id: i64) -> Result<Option<TagRow>> {
		self.row(&format!("SELECT {} FROM {{tags}} WHERE id = ?", TagRow::COLS), [id], TagRow::from)
	}

	fn exact(&self, name: &str) -> Result<Option<TagRow>> {
		self.row(&format!("SELECT {} FROM {{tags}} WHERE name = ?", TagRow::COLS), [self.normalize(name)], TagRow::from)
	}

	/// The tag a name stands for, following an alias to the tag it was merged into.
	fn find(&self, name: &str) -> Result<Option<TagRow>> {
		match self.exact(name)? {
			Some(TagRow { alias_of: Some(to), .. }) => self.tag_by_id(to),
			row => Ok(row),
		}
	}

	/// The canonical name a tag goes by now, if the graph knows it.
	pub fn canonical(&self, name: &str) -> Result<Option<String>> {
		Ok(self.find(name)?.map(|t| t.name))
	}

	/// The tag for a name, creating it if nobody has used it yet.
	fn ensure_tag(&self, raw: &str, actor: &str) -> Result<TagRow> {
		let name = self.normalize(raw);
		if let Some(error) = name_error(&name, &self.opts.namespaces) {
			return refuse(error);
		}
		if let Some(found) = self.find(&name)? {
			if found.kind != "tag" {
				return refuse(format!("“{name}” is a link name, not a tag."));
			}
			return Ok(found);
		}
		self.exec("INSERT INTO {tags} (name, kind, created_by, created_at) VALUES (?, 'tag', ?, ?)", params![name, actor, now()])?;
		let id = self.conn.last_insert_rowid();
		self.log(Some(actor), "new-tag", None, &name)?;
		Ok(self.tag_by_id(id)?.expect("just inserted"))
	}

	// ── Tags on items ───────────────────────────────────────────────────────

	/// Votes a tag on (1) or off (-1) an item, or takes back a vote (0). Voting
	/// a new name on creates it.
	pub fn vote_tag(&self, item: &str, name: &str, actor: &str, vote: Vote) -> Result<()> {
		let tx = self.conn.unchecked_transaction()?;
		let tag = if vote == 1 {
			self.ensure_tag(name, actor)?
		} else {
			let tag = self.find(name)?;
			let on = match &tag {
				Some(t) => self.row("SELECT 1 FROM {item_tags} WHERE item = ? AND tag_id = ?", params![item, t.id], |_| Ok(()))?.is_some(),
				None => false,
			};
			match tag {
				Some(t) if on => t,
				_ => return refuse(format!("“{}” isn't tagged here.", self.normalize(name))),
			}
		};
		if vote == 0 {
			self.exec("DELETE FROM {item_tags} WHERE item = ? AND tag_id = ? AND actor = ?", params![item, tag.id, actor])?;
		} else {
			self.exec(
				"INSERT INTO {item_tags} (item, tag_id, actor, vote, at) VALUES (?, ?, ?, ?, ?)
				ON CONFLICT (item, tag_id, actor) DO UPDATE SET vote = excluded.vote, at = excluded.at",
				params![item, tag.id, actor, vote.signum(), now()],
			)?;
		}
		let action = match vote {
			1 => "tag",
			-1 => "downvote-tag",
			_ => "unvote-tag",
		};
		self.log(Some(actor), action, Some(item), &tag.name)?;
		tx.commit()?;
		Ok(())
	}

	/// The item's tags with a positive score, plus any the viewer voted on. Best first.
	pub fn item_tags(&self, item: &str, viewer: &str) -> Result<Vec<ItemTag>> {
		self.rows(
			"SELECT t.name, SUM(v.vote) score, SUM(CASE WHEN v.actor = ? THEN v.vote ELSE 0 END) mine
			FROM {item_tags} v JOIN {tags} t ON t.id = v.tag_id
			WHERE v.item = ?
			GROUP BY t.id HAVING score > 0 OR mine != 0
			ORDER BY score DESC, t.name",
			params![viewer, item],
			|r| {
				let name: String = r.get(0)?;
				Ok(ItemTag {
					namespace: namespace_of(&name, &self.opts.namespaces).to_string(),
					name,
					score: r.get(1)?,
					mine: r.get::<_, i64>(2)?.signum() as Vote,
				})
			},
		)
	}

	/// Every item's visible tags (score above zero), for a search index. Pass `item` for one.
	pub fn visible_tags(&self, item: Option<&str>) -> Result<BTreeMap<String, Vec<String>>> {
		let text = format!(
			"SELECT v.item, t.name FROM {{item_tags}} v JOIN {{tags}} t ON t.id = v.tag_id
			{} GROUP BY v.item, t.id HAVING SUM(v.vote) > 0",
			if item.is_some() { "WHERE v.item = ?" } else { "" }
		);
		let args: Vec<&str> = item.into_iter().collect();
		let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
		for (item, name) in self.rows(&text, params_from_iter(args), |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
			out.entry(item).or_default().push(name);
		}
		Ok(out)
	}

	/// Most used tags, for suggestions while typing.
	pub fn popular(&self, limit: usize) -> Result<Vec<String>> {
		self.rows(
			"SELECT t.name FROM {tags} t LEFT JOIN {item_tags} v ON v.tag_id = t.id AND v.vote > 0
			WHERE t.kind = 'tag' AND t.alias_of IS NULL
			GROUP BY t.id ORDER BY count(DISTINCT v.item) DESC, t.name LIMIT ?",
			[limit as i64],
			|r| r.get(0),
		)
	}

	// ── Links ───────────────────────────────────────────────────────────────

	pub fn link_names(&self) -> Result<Vec<String>> {
		self.rows("SELECT name FROM {tags} WHERE kind = 'link' AND alias_of IS NULL ORDER BY id", [], |r| r.get(0))
	}

	/// Links tags that are already on the item: its personal tags, plus
	/// whatever the caller passes in `on_item` (the site's own tags). Linking
	/// the same group again counts as a vote for it.
	pub fn add_link(&self, item: &str, kind: &str, members: &[String], actor: &str, on_item: &[String]) -> Result<i64> {
		let k = match self.find(kind)? {
			Some(k) if k.kind == "link" => k,
			_ => return refuse("Pick a link name from the list."),
		};
		let mut names: Vec<String> = Vec::new();
		for m in members {
			let n = self.normalize(m);
			if !n.is_empty() && !names.contains(&n) {
				names.push(n);
			}
		}
		let max = self.opts.max_link_size;
		if names.len() < 2 || names.len() > max {
			return refuse(format!("A link joins 2 to {max} different tags."));
		}

		let mut available: BTreeSet<String> = on_item.iter().map(|t| self.normalize(t)).collect();
		for t in self.item_tags(item, actor)? {
			if t.score > 0 {
				available.insert(t.name);
			}
		}

		let tx = self.conn.unchecked_transaction()?;
		let mut tags = Vec::new();
		for n in &names {
			let canonical = self.canonical(n)?.unwrap_or_else(|| n.clone());
			if !available.contains(n) && !available.contains(&canonical) {
				return refuse(format!("“{n}” isn't a tag here yet. Add it as a tag first."));
			}
			// Already on the item, so the site's tags get a row here too.
			tags.push(self.ensure_tag(n, actor)?);
		}
		let mut ids: Vec<i64> = tags.iter().map(|t| t.id).collect();
		ids.sort_unstable();
		ids.dedup();
		if ids.len() < 2 {
			return refuse("A link needs at least two different tags.");
		}
		let key = ids.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
		let existing = self.row("SELECT id FROM {links} WHERE item = ? AND kind_id = ? AND key = ?", params![item, k.id, key], |r| r.get(0))?;
		let id = match existing {
			Some(id) => id,
			None => {
				self.exec("INSERT INTO {links} (item, kind_id, key, created_by, created_at) VALUES (?, ?, ?, ?, ?)", params![item, k.id, key, actor, now()])?;
				let id = self.conn.last_insert_rowid();
				for (pos, t) in tags.iter().enumerate() {
					self.exec("INSERT INTO {link_members} (link_id, pos, tag_id) VALUES (?, ?, ?)", params![id, pos as i64, t.id])?;
				}
				id
			}
		};
		self.set_link_vote(id, actor, 1)?;
		let detail = format!("{}: {}", k.name, tags.iter().map(|t| t.name.as_str()).collect::<Vec<_>>().join(" · "));
		self.log(Some(actor), "link", Some(item), &detail)?;
		tx.commit()?;
		Ok(id)
	}

	fn set_link_vote(&self, id: i64, actor: &str, vote: Vote) -> Result<()> {
		if vote == 0 {
			self.exec("DELETE FROM {link_votes} WHERE link_id = ? AND actor = ?", params![id, actor])?;
		} else {
			self.exec(
				"INSERT INTO {link_votes} (link_id, actor, vote, at) VALUES (?, ?, ?, ?)
				ON CONFLICT (link_id, actor) DO UPDATE SET vote = excluded.vote, at = excluded.at",
				params![id, actor, vote.signum(), now()],
			)?;
		}
		Ok(())
	}

	pub fn vote_link(&self, id: i64, actor: &str, vote: Vote) -> Result<()> {
		let Some(link) = self.link_by_id(id)? else {
			return refuse("That link is gone.");
		};
		let tx = self.conn.unchecked_transaction()?;
		self.set_link_vote(id, actor, vote)?;
		let action = match vote {
			1 => "upvote-link",
			-1 => "downvote-link",
			_ => "unvote-link",
		};
		self.log(Some(actor), action, Some(&link.item), &format!("{}: {}", link.kind, link.members.join(" · ")))?;
		tx.commit()?;
		Ok(())
	}

	fn link_by_id(&self, id: i64) -> Result<Option<ItemLink>> {
		Ok(self.links("l.id = ?", vec![Value::Integer(id)], "", true)?.pop())
	}

	/// Links matching a WHERE clause, with members in order. Ones voted down only if `all`.
	fn links(&self, cond: &str, args: Vec<Value>, viewer: &str, all: bool) -> Result<Vec<ItemLink>> {
		let text = format!(
			"SELECT l.id, l.item, k.name kind,
				COALESCE(SUM(v.vote), 0) score, COALESCE(SUM(CASE WHEN v.actor = ? THEN v.vote END), 0) mine
			FROM {{links}} l JOIN {{tags}} k ON k.id = l.kind_id LEFT JOIN {{link_votes}} v ON v.link_id = l.id
			WHERE {cond}
			GROUP BY l.id {}
			ORDER BY score DESC, l.id",
			if all { "" } else { "HAVING score > 0 OR mine != 0" }
		);
		let mut all_args = vec![Value::Text(viewer.to_string())];
		all_args.extend(args);
		let mut links = self.rows(&text, params_from_iter(all_args), |r| {
			Ok(ItemLink {
				id: r.get(0)?,
				item: r.get(1)?,
				kind: r.get(2)?,
				members: Vec::new(),
				score: r.get(3)?,
				mine: r.get::<_, i64>(4)?.signum() as Vote,
			})
		})?;
		if links.is_empty() {
			return Ok(links);
		}
		let ids: Vec<i64> = links.iter().map(|l| l.id).collect();
		let mut members: HashMap<i64, Vec<String>> = HashMap::new();
		let text = format!(
			"SELECT m.link_id, t.name FROM {{link_members}} m JOIN {{tags}} t ON t.id = m.tag_id
			WHERE m.link_id IN ({}) ORDER BY m.link_id, m.pos",
			placeholders(ids.len())
		);
		for (id, name) in self.rows(&text, params_from_iter(&ids), |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))? {
			members.entry(id).or_default().push(name);
		}
		for l in &mut links {
			l.members = members.remove(&l.id).unwrap_or_default();
		}
		Ok(links)
	}

	pub fn item_links(&self, item: &str, viewer: &str) -> Result<Vec<ItemLink>> {
		self.links("l.item = ?", vec![Value::Text(item.to_string())], viewer, false)
	}

	/// Items with a link (score above zero) holding every tag asked for, and those
	/// links. `kinds` empty means any link name.
	pub fn find_linked(&self, kinds: &[String], tags: &[String], limit: usize) -> Result<BTreeMap<String, Vec<LinkMatch>>> {
		let mut kind_ids = Vec::new();
		for k in kinds {
			kind_ids.push(self.find(k)?.map_or(-1, |t| t.id));
		}
		let mut tag_ids = Vec::new();
		for t in tags {
			let id = self.find(t)?.map_or(-1, |t| t.id);
			if !tag_ids.contains(&id) {
				tag_ids.push(id);
			}
		}
		let mut cond = vec!["1".to_string()];
		let mut args: Vec<Value> = Vec::new();
		if !kind_ids.is_empty() {
			cond.push(format!("l.kind_id IN ({})", placeholders(kind_ids.len())));
			args.extend(kind_ids.iter().map(|&i| Value::Integer(i)));
		}
		if !tag_ids.is_empty() {
			cond.push(format!(
				"l.id IN (SELECT link_id FROM {{link_members}} WHERE tag_id IN ({}) GROUP BY link_id HAVING count(DISTINCT tag_id) = ?)",
				placeholders(tag_ids.len())
			));
			args.extend(tag_ids.iter().map(|&i| Value::Integer(i)));
			args.push(Value::Integer(tag_ids.len() as i64));
		}
		let mut out: BTreeMap<String, Vec<LinkMatch>> = BTreeMap::new();
		for l in self.links(&cond.join(" AND "), args, "", false)? {
			if l.score <= 0 {
				continue;
			}
			if !out.contains_key(&l.item) && out.len() >= limit {
				break;
			}
			out.entry(l.item).or_default().push(LinkMatch { kind: l.kind, members: l.members });
		}
		Ok(out)
	}

	/// Every link `viewer` has made or agreed with somewhere (their vote is up),
	/// as kind and members, with how many items carry it. These are the
	/// patterns links are filled in from; links nobody agreed with never are.
	pub fn link_patterns(&self, viewer: &str) -> Result<Vec<LinkSuggestion>> {
		let mut found: Vec<LinkSuggestion> = Vec::new();
		let mut confirmed = BTreeSet::new();
		for l in self.links("1", Vec::new(), viewer, false)? {
			if l.score <= 0 {
				continue;
			}
			let mut members = l.members.clone();
			members.sort();
			if l.mine > 0 {
				confirmed.insert((l.kind.clone(), members.clone()));
			}
			match found.iter_mut().find(|s| s.kind == l.kind && s.members == members) {
				Some(s) => s.uses += 1,
				None => found.push(LinkSuggestion { kind: l.kind, members, uses: 1 }),
			}
		}
		found.retain(|s| confirmed.contains(&(s.kind.clone(), s.members.clone())));
		found.sort_by(|a, b| b.uses.cmp(&a.uses).then(b.members.len().cmp(&a.members.len())));
		Ok(found)
	}

	/// Patterns from `link_patterns` that fit an item: all their tags are in
	/// `on_item` (or are the item's own tags here), and it doesn't have that link
	/// yet, not even one voted down.
	pub fn suggest_links(&self, item: &str, on_item: &[String], patterns: &[LinkSuggestion], viewer: &str) -> Result<Vec<LinkSuggestion>> {
		let mut available: BTreeSet<String> = on_item.iter().map(|t| self.normalize(t)).collect();
		for t in self.item_tags(item, viewer)? {
			if t.score > 0 {
				available.insert(t.name);
			}
		}
		let fits: Vec<&LinkSuggestion> = patterns.iter().filter(|p| p.members.iter().all(|m| available.contains(m))).collect();
		if fits.is_empty() {
			return Ok(Vec::new());
		}
		let have: Vec<(String, Vec<String>)> = self
			.links("l.item = ?", vec![Value::Text(item.to_string())], viewer, true)?
			.into_iter()
			.map(|l| {
				let mut m = l.members;
				m.sort();
				(l.kind, m)
			})
			.collect();
		Ok(fits.into_iter().filter(|p| !have.iter().any(|(k, m)| *k == p.kind && *m == p.members)).cloned().collect())
	}

	// ── Editing names ───────────────────────────────────────────────────────

	pub fn remove_item_tag(&self, item: &str, name: &str, actor: &str) -> Result<()> {
		let Some(tag) = self.find(name)? else {
			return refuse("No such tag.");
		};
		let tx = self.conn.unchecked_transaction()?;
		self.exec("DELETE FROM {item_tags} WHERE item = ? AND tag_id = ?", params![item, tag.id])?;
		self.log(Some(actor), "remove-tag", Some(item), &tag.name)?;
		tx.commit()?;
		Ok(())
	}

	pub fn remove_link(&self, id: i64, actor: &str) -> Result<()> {
		let Some(link) = self.link_by_id(id)? else {
			return refuse("That link is gone.");
		};
		let tx = self.conn.unchecked_transaction()?;
		self.exec("DELETE FROM {links} WHERE id = ?", [id])?;
		self.log(Some(actor), "remove-link", Some(&link.item), &format!("{}: {}", link.kind, link.members.join(" · ")))?;
		tx.commit()?;
		Ok(())
	}

	pub fn search_tags(&self, text: &str, limit: usize) -> Result<Vec<TagInfo>> {
		let escaped: String = self.normalize(text).chars().flat_map(|c| if matches!(c, '\\' | '%' | '_') { vec!['\\', c] } else { vec![c] }).collect();
		self.rows(
			"SELECT t.id, t.name, t.kind, a.name target,
				(SELECT count(DISTINCT item) FROM {item_tags} WHERE tag_id = t.id)
				+ (SELECT count(*) FROM {link_members} WHERE tag_id = t.id)
				+ (SELECT count(*) FROM {links} WHERE kind_id = t.id) uses
			FROM {tags} t LEFT JOIN {tags} a ON a.id = t.alias_of
			WHERE t.name LIKE ? ESCAPE '\\' ORDER BY t.kind DESC, uses DESC, t.name LIMIT ?",
			params![format!("%{escaped}%"), limit as i64],
			|r| Ok(TagInfo { id: r.get(0)?, name: r.get(1)?, kind: r.get(2)?, alias_of: r.get(3)?, uses: r.get(4)? }),
		)
	}

	fn must_find(&self, name: &str) -> Result<TagRow> {
		match self.exact(name)? {
			Some(t) => Ok(t),
			None => refuse(format!("No tag called “{}”.", self.normalize(name))),
		}
	}

	pub fn rename(&self, name: &str, to: &str, actor: &str) -> Result<()> {
		let tag = self.must_find(name)?;
		let next = self.normalize(to);
		if let Some(error) = name_error(&next, &self.opts.namespaces) {
			return refuse(error);
		}
		if next == tag.name {
			return Ok(());
		}
		if tag.kind == "link" && tag.name == self.any_link() {
			return refuse(format!("“{}” is the word for any link and can't be renamed.", tag.name));
		}
		if tag.alias_of.is_some() {
			return refuse(format!("“{}” is an alias. Rename the tag it points to instead.", tag.name));
		}
		let tx = self.conn.unchecked_transaction()?;
		// Renaming back to an old name takes that alias over.
		self.exec("DELETE FROM {tags} WHERE name = ? AND alias_of = ?", params![next, tag.id])?;
		if self.row("SELECT 1 FROM {tags} WHERE name = ?", [&next], |_| Ok(()))?.is_some() {
			return refuse(format!("“{next}” already exists. Merge into it instead."));
		}
		self.exec("UPDATE {tags} SET name = ? WHERE id = ?", params![next, tag.id])?;
		// The old name stays as an alias, so searches and typing it still work.
		self.exec(
			"INSERT INTO {tags} (name, kind, alias_of, created_by, created_at) VALUES (?, ?, ?, ?, ?)",
			params![tag.name, tag.kind, tag.id, actor, now()],
		)?;
		self.log(Some(actor), "rename", None, &format!("{} → {next}", tag.name))?;
		tx.commit()?;
		Ok(())
	}

	/// Moves every use of one tag onto another and keeps the old name as an
	/// alias, so typing it still works. Links that become the same are joined.
	pub fn merge(&self, from: &str, into: &str, actor: &str) -> Result<()> {
		let a = self.must_find(from)?;
		let b = match self.find(into)? {
			Some(b) => b,
			None => self.must_find(into)?,
		};
		if a.id == b.id {
			return refuse("Those are already the same tag.");
		}
		if a.kind != b.kind {
			return refuse("A tag and a link name can't be merged.");
		}
		if a.kind == "link" && a.name == self.any_link() {
			return refuse(format!("“{}” is the word for any link and can't be merged away.", a.name));
		}
		let tx = self.conn.unchecked_transaction()?;
		self.exec(
			"INSERT OR IGNORE INTO {item_tags} (item, tag_id, actor, vote, at) SELECT item, ?, actor, vote, at FROM {item_tags} WHERE tag_id = ?",
			params![b.id, a.id],
		)?;
		self.exec("DELETE FROM {item_tags} WHERE tag_id = ?", [a.id])?;
		if a.kind == "link" {
			for (id, item, key) in self.rows("SELECT id, item, key FROM {links} WHERE kind_id = ?", [a.id], |r| {
				Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?))
			})? {
				let twin: Option<i64> = self.row("SELECT id FROM {links} WHERE item = ? AND kind_id = ? AND key = ?", params![item, b.id, key], |r| r.get(0))?;
				match twin {
					Some(twin) => self.fold(id, twin)?,
					None => {
						self.exec("UPDATE {links} SET kind_id = ? WHERE id = ?", params![b.id, id])?;
					}
				}
			}
		} else {
			let affected: Vec<i64> = self.rows("SELECT DISTINCT link_id FROM {link_members} WHERE tag_id = ?", [a.id], |r| r.get(0))?;
			self.exec("UPDATE {link_members} SET tag_id = ? WHERE tag_id = ?", params![b.id, a.id])?;
			for id in affected {
				self.rekey(id)?;
			}
		}
		self.exec("UPDATE {tags} SET alias_of = ? WHERE id = ? OR alias_of = ?", params![b.id, a.id, a.id])?;
		self.log(Some(actor), "merge", None, &format!("{} → {}", a.name, b.name))?;
		tx.commit()?;
		Ok(())
	}

	/// After a merge: drop repeated members, and fold the link into an identical one.
	fn rekey(&self, id: i64) -> Result<()> {
		let Some((item, kind_id)) = self.row("SELECT item, kind_id FROM {links} WHERE id = ?", [id], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))? else {
			return Ok(());
		};
		let members: Vec<(i64, i64)> = self.rows("SELECT pos, tag_id FROM {link_members} WHERE link_id = ? ORDER BY pos", [id], |r| Ok((r.get(0)?, r.get(1)?)))?;
		let mut seen = BTreeSet::new();
		for (pos, tag_id) in members {
			if !seen.insert(tag_id) {
				self.exec("DELETE FROM {link_members} WHERE link_id = ? AND pos = ?", params![id, pos])?;
			}
		}
		if seen.len() < 2 {
			self.exec("DELETE FROM {links} WHERE id = ?", [id])?;
			return Ok(());
		}
		let key = seen.iter().map(i64::to_string).collect::<Vec<_>>().join(",");
		let twin: Option<i64> = self.row(
			"SELECT id FROM {links} WHERE item = ? AND kind_id = ? AND key = ? AND id != ?",
			params![item, kind_id, key, id],
			|r| r.get(0),
		)?;
		match twin {
			Some(twin) => self.fold(id, twin)?,
			None => {
				self.exec("UPDATE {links} SET key = ? WHERE id = ?", params![key, id])?;
			}
		}
		Ok(())
	}

	/// Moves one link's votes onto its twin and deletes it.
	fn fold(&self, id: i64, into: i64) -> Result<()> {
		self.exec(
			"INSERT OR IGNORE INTO {link_votes} (link_id, actor, vote, at) SELECT ?, actor, vote, at FROM {link_votes} WHERE link_id = ?",
			params![into, id],
		)?;
		self.exec("DELETE FROM {links} WHERE id = ?", [id])?;
		Ok(())
	}

	pub fn add_link_name(&self, name: &str, actor: &str) -> Result<()> {
		let n = self.normalize(name);
		if let Some(error) = name_error(&n, &self.opts.namespaces) {
			return refuse(error);
		}
		if !n.chars().all(|c| c.is_alphanumeric() || c == '_') {
			return refuse("Link names use letters, numbers and _ only, so searches can spot them.");
		}
		if self.find(&n)?.is_some() {
			return refuse(format!("“{n}” is already a tag or link name."));
		}
		let tx = self.conn.unchecked_transaction()?;
		self.exec("INSERT INTO {tags} (name, kind, created_by, created_at) VALUES (?, 'link', ?, ?)", params![n, actor, now()])?;
		self.log(Some(actor), "new-link-name", None, &n)?;
		tx.commit()?;
		Ok(())
	}

	/// Takes back every vote an actor made, dropping links nobody else backs.
	pub fn forget(&self, actor: &str) -> Result<()> {
		let tx = self.conn.unchecked_transaction()?;
		self.exec("DELETE FROM {item_tags} WHERE actor = ?", [actor])?;
		self.exec("DELETE FROM {link_votes} WHERE actor = ?", [actor])?;
		self.exec("DELETE FROM {links} WHERE NOT EXISTS (SELECT 1 FROM {link_votes} v WHERE v.link_id = {links}.id)", [])?;
		self.exec("UPDATE {log} SET actor = NULL WHERE actor = ?", [actor])?;
		self.exec("UPDATE {tags} SET created_by = NULL WHERE created_by = ?", [actor])?;
		tx.commit()?;
		Ok(())
	}

	pub fn recent(&self, limit: usize, actor: Option<&str>) -> Result<Vec<LogEntry>> {
		let f = |r: &Row| Ok(LogEntry { id: r.get(0)?, at: r.get(1)?, actor: r.get(2)?, action: r.get(3)?, item: r.get(4)?, detail: r.get(5)? });
		match actor {
			Some(a) => self.rows("SELECT id, at, actor, action, item, detail FROM {log} WHERE actor = ? ORDER BY id DESC LIMIT ?", params![a, limit as i64], f),
			None => self.rows("SELECT id, at, actor, action, item, detail FROM {log} ORDER BY id DESC LIMIT ?", [limit as i64], f),
		}
	}

	pub fn stats(&self) -> Result<Stats> {
		Ok(Stats {
			tags: self.count("SELECT count(*) FROM {tags} WHERE kind = 'tag' AND alias_of IS NULL")?,
			link_names: self.count("SELECT count(*) FROM {tags} WHERE kind = 'link' AND alias_of IS NULL")?,
			tagged_items: self.count("SELECT count(DISTINCT item) FROM {item_tags}")?,
			links: self.count("SELECT count(*) FROM {links}")?,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn setup() -> (Connection, Options) {
		let conn = crate::wipe::open_in_memory().unwrap();
		conn.execute_batch("PRAGMA foreign_keys = ON").unwrap();
		let opts = Options::default();
		TagGraph::migrate(&conn, &opts).unwrap();
		(conn, opts)
	}

	fn s(v: &[&str]) -> Vec<String> {
		v.iter().map(|x| x.to_string()).collect()
	}

	#[test]
	fn tags_and_links() {
		let (conn, opts) = setup();
		let g = TagGraph::new(&conn, &opts);
		g.vote_tag("e621:post/1", "Drink", "me", 1).unwrap();
		assert_eq!(g.item_tags("e621:post/1", "me").unwrap()[0].name, "drink");

		// `cow` and `milk` are the site's tags; `drink` is ours.
		let id = g.add_link("e621:post/1", "relation", &s(&["cow", "drink", "milk"]), "me", &s(&["cow", "milk"])).unwrap();
		let links = g.item_links("e621:post/1", "me").unwrap();
		assert_eq!(links[0].members, s(&["cow", "drink", "milk"]));
		assert_eq!(links[0].mine, 1);

		assert!(g.find_linked(&[], &s(&["drink"]), 10).unwrap().contains_key("e621:post/1"));
		assert!(g.find_linked(&s(&["relation"]), &s(&["cow", "milk"]), 10).unwrap().contains_key("e621:post/1"));
		assert!(g.find_linked(&s(&["relation"]), &s(&["cow", "carrot"]), 10).unwrap().is_empty());

		g.vote_link(id, "me", -1).unwrap();
		assert!(g.find_linked(&[], &s(&["drink"]), 10).unwrap().is_empty());
	}

	#[test]
	fn links_need_tags_on_the_item() {
		let (conn, opts) = setup();
		let g = TagGraph::new(&conn, &opts);
		let err = g.add_link("x", "relation", &s(&["a", "b"]), "me", &s(&["a"])).unwrap_err();
		assert!(err.to_string().contains("isn't a tag here"));
		assert!(g.add_link("x", "nope", &s(&["a", "b"]), "me", &s(&["a", "b"])).is_err());
		assert!(g.add_link("x", "relation", &s(&["a"]), "me", &s(&["a"])).is_err());
	}

	#[test]
	fn rename_and_merge_keep_aliases() {
		let (conn, opts) = setup();
		let g = TagGraph::new(&conn, &opts);
		g.vote_tag("x", "kitty", "me", 1).unwrap();
		g.rename("kitty", "cat", "me").unwrap();
		assert_eq!(g.canonical("kitty").unwrap().as_deref(), Some("cat"));

		g.vote_tag("y", "feline", "me", 1).unwrap();
		g.vote_tag("y", "dog", "me", 1).unwrap();
		g.vote_tag("y", "cat", "me", 1).unwrap();
		g.add_link("y", "relation", &s(&["feline", "dog"]), "me", &[]).unwrap();
		g.add_link("y", "relation", &s(&["cat", "dog"]), "me", &[]).unwrap();
		g.merge("feline", "cat", "me").unwrap();
		// The two links became one.
		assert_eq!(g.item_links("y", "me").unwrap().len(), 1);
		assert_eq!(g.canonical("feline").unwrap().as_deref(), Some("cat"));
		let mut tags = g.visible_tags(Some("y")).unwrap().remove("y").unwrap();
		tags.sort();
		assert_eq!(tags, s(&["cat", "dog"]));
	}

	#[test]
	fn link_names() {
		let (conn, opts) = setup();
		let g = TagGraph::new(&conn, &opts);
		g.add_link_name("holds", "me").unwrap();
		assert_eq!(g.link_names().unwrap(), s(&["link", "relation", "holds"]));
		assert!(g.add_link_name("holds!", "me").is_err());
		assert!(g.rename("link", "any", "me").is_err());
	}

	#[test]
	fn suggests_links_made_before() {
		let conn = crate::wipe::open_in_memory().unwrap();
		let opts = Options::default();
		TagGraph::migrate(&conn, &opts).unwrap();
		let g = TagGraph::new(&conn, &opts);
		let tags = |s: &str| s.split(' ').map(String::from).collect::<Vec<_>>();
		g.add_link("a", "relation", &tags("rabbit carrot"), "me", &tags("rabbit carrot eat")).unwrap();
		let patterns = g.link_patterns("me").unwrap();
		assert_eq!(patterns.len(), 1);
		// Another item with both tags gets it offered; one missing a tag doesn't.
		let s = g.suggest_links("b", &tags("carrot rabbit field"), &patterns, "me").unwrap();
		assert_eq!((s[0].kind.as_str(), s[0].members.clone()), ("relation", tags("carrot rabbit")));
		assert!(g.suggest_links("c", &tags("rabbit"), &patterns, "me").unwrap().is_empty());
		// Filled in by someone else and turned down: not offered again, and not a pattern.
		let id = g.add_link("b", "relation", &tags("rabbit carrot"), "auto", &tags("rabbit carrot")).unwrap();
		g.vote_link(id, "me", -1).unwrap();
		assert!(g.suggest_links("b", &tags("carrot rabbit"), &patterns, "me").unwrap().is_empty());
		assert_eq!(g.link_patterns("me").unwrap()[0].uses, 1);
		// A pattern only "auto" agreed with isn't one.
		g.add_link("d", "link", &tags("fox den"), "auto", &tags("fox den")).unwrap();
		assert_eq!(g.link_patterns("me").unwrap().len(), 1);
	}

}
