import type BetterSqlite3 from 'better-sqlite3';
import { nameError, namespaceOf, normalizeName, words } from './names';

// Community tags and links between them, kept in SQLite.
//
// Items are opaque keys ("anilist:21", "booru:post/991"): the graph never
// knows what they are. People put tags on items and vote on them. A link is a
// named group of tags that are already on one item, e.g. a "relation" link of
// rabbit · eat · carrot, so a search for "eat link" finds every item where
// eat is linked to something. Link names are tags too, of kind "link", and
// are made by moderators so they stay few.

type DB = BetterSqlite3.Database;

/** Whoever is making a change, as the host app sees them. */
export interface Actor {
	id: string;
	/** May see and use adult tags. */
	adult: boolean;
	/** May create tag names nobody has used yet (hosts usually wait a day after sign-up). */
	canCreate: boolean;
	moderator?: boolean;
}

/** Whoever is looking: their own votes are marked, adult tags hidden unless `adult`. */
export interface Viewer {
	id?: string;
	adult: boolean;
}

export type Vote = -1 | 0 | 1;

export interface ItemTag {
	name: string;
	namespace: string;
	adult: boolean;
	score: number;
	mine: Vote;
}

export interface ItemLink {
	id: number;
	kind: string;
	members: string[];
	adult: boolean;
	score: number;
	mine: Vote;
}

export interface TagInfo {
	id: number;
	name: string;
	kind: 'tag' | 'link';
	adult: boolean;
	banned: boolean;
	aliasOf: string | null;
	/** Items it's on, plus links it's in. */
	uses: number;
}

export interface LogEntry {
	id: number;
	at: number;
	actor: string | null;
	action: string;
	item: string | null;
	detail: string;
}

/** A search for linked tags: "rabbit eat link" → kinds [link], tags [rabbit, eat]. */
export interface LinkQuery {
	/** Link names asked for; empty means any kind (the `anyLink` word). */
	kinds: string[];
	tags: string[];
	/** Words that were neither, for the host to search on its own. */
	rest: string;
}

export interface LinkMatch {
	kind: string;
	members: string[];
}

export interface Limits {
	/** Changes one account may make per hour (moderators are exempt). */
	perHour: number;
	/** New tag names one account may create per day. */
	newPerDay: number;
	/** New tag names the whole site may get per hour. */
	newSitePerHour: number;
	/** Most tags one link may join. */
	maxLinkSize: number;
}

export interface TagGraphOptions {
	/** Table name prefix, so several graphs (or apps) can share a database. */
	prefix?: string;
	/** Namespaces a tag may start with, e.g. "character:rem". */
	namespaces?: string[];
	/** Link names created on first run. The first is also the search word for "any link". */
	linkNames?: string[];
	limits?: Partial<Limits>;
}

/** A refusal to show the person, e.g. a bad tag name or a rate limit. */
export class TagGraphError extends Error {}

interface TagRow {
	id: number;
	name: string;
	kind: 'tag' | 'link';
	adult: number;
	alias_of: number | null;
	banned: number;
}

const HOUR = 3600_000;
const DAY = 24 * HOUR;

const SCHEMA = [
	`
	CREATE TABLE {tags} (
		id INTEGER PRIMARY KEY,
		name TEXT NOT NULL UNIQUE,
		kind TEXT NOT NULL DEFAULT 'tag',
		adult INTEGER NOT NULL DEFAULT 0,
		alias_of INTEGER REFERENCES {tags}(id) ON DELETE SET NULL,
		banned INTEGER NOT NULL DEFAULT 0,
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
	CREATE TABLE {blocked} (
		actor TEXT PRIMARY KEY,
		at INTEGER NOT NULL
	);
	CREATE TABLE {log} (
		id INTEGER PRIMARY KEY,
		at INTEGER NOT NULL,
		actor TEXT,
		action TEXT NOT NULL,
		item TEXT,
		detail TEXT NOT NULL
	);
	CREATE INDEX {log}_actor ON {log}(actor, at);
	`
];

export class TagGraph {
	readonly namespaces: readonly string[];
	readonly limits: Limits;
	/** The search word meaning "linked by anything". */
	readonly anyLink: string;
	private stmts = new Map<string, BetterSqlite3.Statement>();
	private listeners: ((items: string[] | null) => void)[] = [];
	private phrases: { tags: Map<string, TagRow>; kinds: Map<string, TagRow> } | null = null;

	constructor(
		private db: DB,
		private options: TagGraphOptions = {}
	) {
		this.namespaces = options.namespaces ?? ['character', 'meta'];
		this.limits = { perHour: 30, newPerDay: 10, newSitePerHour: 60, maxLinkSize: 5, ...options.limits };
		const linkNames = options.linkNames ?? ['link', 'relation'];
		this.anyLink = linkNames[0];
		this.migrate(linkNames);
	}

	// ── Plumbing ────────────────────────────────────────────────────────────

	private sql(text: string) {
		const p = this.options.prefix ?? 'tg';
		return text.replace(/\{(\w+)\}/g, (_, t) => `${p}_${t}`);
	}

	private st(text: string) {
		let s = this.stmts.get(text);
		if (!s) this.stmts.set(text, (s = this.db.prepare(this.sql(text))));
		return s;
	}

	private all<T>(text: string, ...args: unknown[]) {
		return this.st(text).all(...args) as T[];
	}

	private get<T>(text: string, ...args: unknown[]) {
		return this.st(text).get(...args) as T | undefined;
	}

	private run(text: string, ...args: unknown[]) {
		return this.st(text).run(...args);
	}

	private migrate(linkNames: string[]) {
		this.db.exec(this.sql('CREATE TABLE IF NOT EXISTS {meta} (key TEXT PRIMARY KEY, value TEXT NOT NULL)'));
		const row = this.get<{ value: string }>("SELECT value FROM {meta} WHERE key = 'version'");
		const version = row ? Number(row.value) : 0;
		for (let i = version; i < SCHEMA.length; i++) {
			this.db.transaction(() => {
				this.db.exec(this.sql(SCHEMA[i]));
				this.run("INSERT INTO {meta} (key, value) VALUES ('version', ?) ON CONFLICT (key) DO UPDATE SET value = excluded.value", String(i + 1));
				if (i === 0) {
					for (const n of linkNames) this.run("INSERT INTO {tags} (name, kind, created_at) VALUES (?, 'link', ?)", n, Date.now());
				}
			})();
		}
	}

	/** Called with the items whose tags or links changed, or null when it could be any. */
	onChange(fn: (items: string[] | null) => void) {
		this.listeners.push(fn);
	}

	private changed(items: string[] | null) {
		this.phrases = null;
		for (const fn of this.listeners) fn(items);
	}

	private log(actor: string | null, action: string, item: string | null, detail: string) {
		this.run('INSERT INTO {log} (at, actor, action, item, detail) VALUES (?, ?, ?, ?, ?)', Date.now(), actor, action, item, detail);
	}

	normalize(name: string) {
		return normalizeName(name, this.namespaces);
	}

	/** The tag a name stands for, following an alias to the tag it was merged into. */
	private find(name: string): TagRow | undefined {
		const row = this.get<TagRow>('SELECT * FROM {tags} WHERE name = ?', this.normalize(name));
		if (row?.alias_of) return this.get<TagRow>('SELECT * FROM {tags} WHERE id = ?', row.alias_of);
		return row;
	}

	/** Refuses blocked accounts and anyone past the hourly limit. */
	private allow(actor: Actor) {
		if (this.get('SELECT 1 FROM {blocked} WHERE actor = ?', actor.id)) {
			throw new TagGraphError('Tagging is turned off for your account.');
		}
		if (actor.moderator) return;
		const { n } = this.get<{ n: number }>("SELECT count(*) n FROM {log} WHERE actor = ? AND at > ? AND action != 'new-tag'", actor.id, Date.now() - HOUR)!;
		if (n >= this.limits.perHour) throw new TagGraphError("That's a lot of tagging for one hour. Try again a little later.");
	}

	/** The tag for a name, creating it when allowed. */
	private ensureTag(raw: string, actor: Actor, adult: boolean, create: 'checked' | 'free'): TagRow {
		const name = this.normalize(raw);
		const error = nameError(name, this.namespaces);
		if (error) throw new TagGraphError(error);
		const found = this.find(name);
		if (found) {
			if (found.banned) throw new TagGraphError(`“${name}” can't be used as a tag.`);
			if (found.kind !== 'tag') throw new TagGraphError(`“${name}” is a link name, not a tag.`);
			if (found.adult && !actor.adult) throw new TagGraphError(`“${name}” is an adult tag.`);
			return found;
		}
		if (create === 'checked') {
			if (!actor.canCreate) throw new TagGraphError(`Nobody has used “${name}” yet, and new accounts can only pick existing tags at first.`);
			if (!actor.moderator) {
				const count = (since: number, who?: string) =>
					who
						? this.get<{ n: number }>('SELECT count(*) n FROM {tags} WHERE created_by = ? AND created_at > ?', who, since)!.n
						: this.get<{ n: number }>('SELECT count(*) n FROM {tags} WHERE created_at > ?', since)!.n;
				if (count(Date.now() - DAY, actor.id) >= this.limits.newPerDay) {
					throw new TagGraphError("You've made a lot of new tags today. Try an existing one, or come back tomorrow.");
				}
				if (count(Date.now() - HOUR) >= this.limits.newSitePerHour) {
					throw new TagGraphError('Lots of new tags are being made right now. Try again in a while.');
				}
			}
		}
		const { lastInsertRowid } = this.run(
			"INSERT INTO {tags} (name, kind, adult, created_by, created_at) VALUES (?, 'tag', ?, ?, ?)",
			name,
			adult && actor.adult ? 1 : 0,
			actor.id,
			Date.now()
		);
		this.log(actor.id, 'new-tag', null, name);
		return this.get<TagRow>('SELECT * FROM {tags} WHERE id = ?', lastInsertRowid)!;
	}

	// ── Tags on items ───────────────────────────────────────────────────────

	/**
	 * Votes a tag on (1) or off (-1) an item, or takes back a vote (0). Voting
	 * a new name on creates it; `adult` marks a tag being created as adult.
	 */
	voteTag(item: string, name: string, actor: Actor, vote: Vote, adult = false) {
		this.allow(actor);
		this.db.transaction(() => {
			let tag: TagRow | undefined;
			if (vote === 1) tag = this.ensureTag(name, actor, adult, 'checked');
			else {
				tag = this.find(name);
				const on = tag && this.get('SELECT 1 FROM {item_tags} WHERE item = ? AND tag_id = ?', item, tag.id);
				if (!tag || !on || (tag.adult && !actor.adult)) throw new TagGraphError(`“${this.normalize(name)}” isn't tagged here.`);
			}
			if (vote === 0) this.run('DELETE FROM {item_tags} WHERE item = ? AND tag_id = ? AND actor = ?', item, tag.id, actor.id);
			else
				this.run(
					'INSERT INTO {item_tags} (item, tag_id, actor, vote, at) VALUES (?, ?, ?, ?, ?) ON CONFLICT (item, tag_id, actor) DO UPDATE SET vote = excluded.vote, at = excluded.at',
					item,
					tag.id,
					actor.id,
					vote,
					Date.now()
				);
			this.log(actor.id, vote === 1 ? 'tag' : vote === -1 ? 'downvote-tag' : 'unvote-tag', item, tag.name);
		})();
		this.changed([item]);
	}

	/** The item's tags with a positive score, plus any the viewer voted on. Best first. */
	itemTags(item: string, viewer: Viewer): ItemTag[] {
		const rows = this.all<{ name: string; adult: number; score: number; mine: number }>(
			`SELECT t.name, t.adult, SUM(v.vote) score, SUM(CASE WHEN v.actor = ? THEN v.vote ELSE 0 END) mine
			FROM {item_tags} v JOIN {tags} t ON t.id = v.tag_id
			WHERE v.item = ? AND t.banned = 0
			GROUP BY t.id HAVING score > 0 OR mine != 0
			ORDER BY score DESC, t.name`,
			viewer.id ?? '',
			item
		);
		return rows
			.filter((r) => viewer.adult || !r.adult)
			.map((r) => ({
				name: r.name,
				namespace: namespaceOf(r.name, this.namespaces),
				adult: !!r.adult,
				score: r.score,
				mine: Math.sign(r.mine) as Vote
			}));
	}

	/** Every item's visible tags (score above zero), for a search index. Pass `item` for one. */
	visibleTags(item?: string): Map<string, { name: string; adult: boolean }[]> {
		const rows = this.all<{ item: string; name: string; adult: number }>(
			`SELECT v.item, t.name, t.adult FROM {item_tags} v JOIN {tags} t ON t.id = v.tag_id
			WHERE t.banned = 0 ${item === undefined ? '' : 'AND v.item = ?'}
			GROUP BY v.item, t.id HAVING SUM(v.vote) > 0`,
			...(item === undefined ? [] : [item])
		);
		const out = new Map<string, { name: string; adult: boolean }[]>();
		for (const r of rows) {
			let list = out.get(r.item);
			if (!list) out.set(r.item, (list = []));
			list.push({ name: r.name, adult: !!r.adult });
		}
		return out;
	}

	/** Most used tags, for suggestions while typing. */
	popular(limit: number, adult: boolean): string[] {
		return this.all<{ name: string }>(
			`SELECT t.name FROM {tags} t LEFT JOIN {item_tags} v ON v.tag_id = t.id AND v.vote > 0
			WHERE t.kind = 'tag' AND t.banned = 0 AND t.alias_of IS NULL AND (? OR t.adult = 0)
			GROUP BY t.id ORDER BY count(DISTINCT v.item) DESC, t.name LIMIT ?`,
			adult ? 1 : 0,
			limit
		).map((r) => r.name);
	}

	// ── Links ───────────────────────────────────────────────────────────────

	linkNames(adult: boolean): string[] {
		return this.all<{ name: string }>(
			"SELECT name FROM {tags} WHERE kind = 'link' AND banned = 0 AND alias_of IS NULL AND (? OR adult = 0) ORDER BY id",
			adult ? 1 : 0
		).map((r) => r.name);
	}

	/**
	 * Links tags that are already on the item: its own community tags, plus
	 * whatever the host passes in `onItem` (e.g. the catalog's own tags).
	 * Linking the same group again counts as a vote for it.
	 */
	addLink(item: string, kind: string, members: string[], actor: Actor, onItem: { name: string; adult?: boolean }[] = []): number {
		this.allow(actor);
		const k = this.find(kind);
		if (!k || k.kind !== 'link' || k.banned || (k.adult && !actor.adult)) throw new TagGraphError('Pick a link name from the list.');
		const names = [...new Set(members.map((m) => this.normalize(m)).filter(Boolean))];
		if (names.length < 2 || names.length > this.limits.maxLinkSize) {
			throw new TagGraphError(`A link joins 2 to ${this.limits.maxLinkSize} different tags.`);
		}

		const available = new Map<string, boolean>();
		for (const t of onItem) available.set(this.normalize(t.name), !!t.adult);
		for (const t of this.itemTags(item, { adult: actor.adult })) if (t.score > 0) available.set(t.name, t.adult);

		let id = 0;
		this.db.transaction(() => {
			const tags = names.map((n) => {
				const canonical = this.find(n)?.name ?? n;
				if (!available.has(n) && !available.has(canonical)) throw new TagGraphError(`“${n}” isn't a tag here yet. Add it as a tag first.`);
				// Already on the item, so no new-name limits; catalog tags get a row here.
				return this.ensureTag(n, actor, available.get(n) ?? available.get(canonical) ?? false, 'free');
			});
			if (new Set(tags.map((t) => t.id)).size < 2) throw new TagGraphError('A link needs at least two different tags.');
			const key = tags.map((t) => t.id).sort((a, b) => a - b).join(',');
			const existing = this.get<{ id: number }>('SELECT id FROM {links} WHERE item = ? AND kind_id = ? AND key = ?', item, k.id, key);
			if (existing) id = existing.id;
			else {
				id = Number(
					this.run('INSERT INTO {links} (item, kind_id, key, created_by, created_at) VALUES (?, ?, ?, ?, ?)', item, k.id, key, actor.id, Date.now())
						.lastInsertRowid
				);
				tags.forEach((t, pos) => this.run('INSERT INTO {link_members} (link_id, pos, tag_id) VALUES (?, ?, ?)', id, pos, t.id));
			}
			this.setLinkVote(id, actor.id, 1);
			this.log(actor.id, 'link', item, `${k.name}: ${tags.map((t) => t.name).join(' · ')}`);
		})();
		this.changed([item]);
		return id;
	}

	private setLinkVote(id: number, actor: string, vote: Vote) {
		if (vote === 0) this.run('DELETE FROM {link_votes} WHERE link_id = ? AND actor = ?', id, actor);
		else
			this.run(
				'INSERT INTO {link_votes} (link_id, actor, vote, at) VALUES (?, ?, ?, ?) ON CONFLICT (link_id, actor) DO UPDATE SET vote = excluded.vote, at = excluded.at',
				id,
				actor,
				vote,
				Date.now()
			);
	}

	voteLink(id: number, actor: Actor, vote: Vote) {
		this.allow(actor);
		const link = this.linkById(id, { adult: actor.adult });
		if (!link) throw new TagGraphError('That link is gone.');
		this.db.transaction(() => {
			this.setLinkVote(id, actor.id, vote);
			this.log(actor.id, vote === 1 ? 'upvote-link' : vote === -1 ? 'downvote-link' : 'unvote-link', link.item, `${link.kind}: ${link.members.join(' · ')}`);
		})();
		this.changed([link.item]);
	}

	private linkById(id: number, viewer: Viewer) {
		const row = this.get<{ item: string }>('SELECT item FROM {links} WHERE id = ?', id);
		const link = row && this.links(`l.id = ?`, [id], viewer, true)[0];
		return link ? { ...link, item: row.item } : null;
	}

	/** Links matching a WHERE clause, with members in order. Hidden ones only if `all`. */
	private links(where: string, args: unknown[], viewer: Viewer, all = false): (ItemLink & { item: string })[] {
		const rows = this.all<{ id: number; item: string; kind: string; kadult: number; score: number; mine: number }>(
			`SELECT l.id, l.item, k.name kind, k.adult kadult,
				COALESCE(SUM(v.vote), 0) score, COALESCE(SUM(CASE WHEN v.actor = ? THEN v.vote END), 0) mine
			FROM {links} l JOIN {tags} k ON k.id = l.kind_id LEFT JOIN {link_votes} v ON v.link_id = l.id
			WHERE ${where} AND k.banned = 0
			GROUP BY l.id ${all ? '' : 'HAVING score > 0 OR mine != 0'}
			ORDER BY score DESC, l.id`,
			viewer.id ?? '',
			...args
		);
		if (!rows.length) return [];
		const members = new Map<number, { name: string; adult: number; banned: number }[]>();
		for (const m of this.all<{ link_id: number; name: string; adult: number; banned: number }>(
			`SELECT m.link_id, t.name, t.adult, t.banned FROM {link_members} m JOIN {tags} t ON t.id = m.tag_id
			WHERE m.link_id IN (${rows.map(() => '?').join(',')}) ORDER BY m.link_id, m.pos`,
			...rows.map((r) => r.id)
		)) {
			let list = members.get(m.link_id);
			if (!list) members.set(m.link_id, (list = []));
			list.push(m);
		}
		return rows.flatMap((r) => {
			const ms = members.get(r.id) ?? [];
			if (ms.some((m) => m.banned)) return [];
			const adult = !!r.kadult || ms.some((m) => m.adult);
			if (adult && !viewer.adult) return [];
			return [{ id: r.id, item: r.item, kind: r.kind, members: ms.map((m) => m.name), adult, score: r.score, mine: Math.sign(r.mine) as Vote }];
		});
	}

	itemLinks(item: string, viewer: Viewer): ItemLink[] {
		return this.links('l.item = ?', [item], viewer);
	}

	// ── Searching ───────────────────────────────────────────────────────────

	private phraseMaps() {
		if (this.phrases) return this.phrases;
		const tags = new Map<string, TagRow>();
		const kinds = new Map<string, TagRow>();
		const rows = this.all<TagRow & { target: string | null }>('SELECT t.*, a.name target FROM {tags} t LEFT JOIN {tags} a ON a.id = t.alias_of WHERE t.banned = 0');
		const byId = new Map(rows.map((r) => [r.id, r]));
		for (const r of rows) {
			const canonical = r.alias_of ? byId.get(r.alias_of) : r;
			if (!canonical) continue;
			(r.kind === 'link' ? kinds : tags).set(words(r.name).join(' '), canonical);
		}
		this.phrases = { tags, kinds };
		return this.phrases;
	}

	/**
	 * Reads a search as a link search when it names a link: "eat link",
	 * "rabbit eat relation". Returns null when it doesn't name one.
	 */
	parse(text: string, adult: boolean): LinkQuery | null {
		const { tags, kinds } = this.phraseMaps();
		const ws = words(text).slice(0, 12);
		const q: LinkQuery = { kinds: [], tags: [], rest: '' };
		const rest: string[] = [];
		let any = false;
		let named = false;
		for (let i = 0; i < ws.length; ) {
			let took = 0;
			for (let n = Math.min(4, ws.length - i); n >= 1 && !took; n--) {
				const phrase = ws.slice(i, i + n).join(' ');
				const kind = kinds.get(phrase);
				const tag = tags.get(phrase) ?? (n === 1 && phrase.length > 3 && phrase.endsWith('s') ? tags.get(phrase.slice(0, -1)) : undefined);
				if (kind && (adult || !kind.adult)) {
					named = true;
					if (kind.name === this.anyLink) any = true;
					else q.kinds.push(kind.name);
					took = n;
				} else if (tag && (adult || !tag.adult)) {
					q.tags.push(tag.name);
					took = n;
				}
			}
			if (!took) rest.push(ws[i]);
			i += took || 1;
		}
		if (!named) return null;
		if (any) q.kinds = [];
		q.tags = [...new Set(q.tags)];
		q.rest = rest.join(' ');
		return q;
	}

	/** Items with a visible link holding every tag asked for, and those links. */
	findLinked(query: Pick<LinkQuery, 'kinds' | 'tags'>, viewer: Viewer, limit = 5000): Map<string, LinkMatch[]> {
		const kindIds = query.kinds.map((k) => this.find(k)?.id ?? -1);
		const tagIds = [...new Set(query.tags.map((t) => this.find(t)?.id ?? -1))];
		const where = ['1'];
		const args: unknown[] = [];
		if (kindIds.length) {
			where.push(`l.kind_id IN (${kindIds.map(() => '?').join(',')})`);
			args.push(...kindIds);
		}
		if (tagIds.length) {
			where.push(
				`l.id IN (SELECT link_id FROM {link_members} WHERE tag_id IN (${tagIds.map(() => '?').join(',')}) GROUP BY link_id HAVING count(DISTINCT tag_id) = ?)`
			);
			args.push(...tagIds, tagIds.length);
		}
		// One statement per shape of query; there are only a few.
		const out = new Map<string, LinkMatch[]>();
		for (const l of this.links(where.join(' AND '), args, { adult: viewer.adult })) {
			if (l.score <= 0) continue;
			let list = out.get(l.item);
			if (!list) {
				if (out.size >= limit) break;
				out.set(l.item, (list = []));
			}
			list.push({ kind: l.kind, members: l.members });
		}
		return out;
	}

	// ── Moderation ──────────────────────────────────────────────────────────

	removeItemTag(item: string, name: string, mod: Actor) {
		const tag = this.find(name);
		if (!tag) throw new TagGraphError('No such tag.');
		this.db.transaction(() => {
			this.run('DELETE FROM {item_tags} WHERE item = ? AND tag_id = ?', item, tag.id);
			this.log(mod.id, 'remove-tag', item, tag.name);
		})();
		this.changed([item]);
	}

	removeLink(id: number, mod: Actor) {
		const link = this.linkById(id, { adult: true });
		if (!link) throw new TagGraphError('That link is gone.');
		this.db.transaction(() => {
			this.run('DELETE FROM {links} WHERE id = ?', id);
			this.log(mod.id, 'remove-link', link.item, `${link.kind}: ${link.members.join(' · ')}`);
		})();
		this.changed([link.item]);
	}

	searchTags(text: string, limit = 50): TagInfo[] {
		const like = `%${this.normalize(text).replace(/[\\%_]/g, (c) => '\\' + c)}%`;
		return this.all<TagRow & { target: string | null; uses: number }>(
			`SELECT t.*, a.name target,
				(SELECT count(DISTINCT item) FROM {item_tags} WHERE tag_id = t.id)
				+ (SELECT count(*) FROM {link_members} WHERE tag_id = t.id)
				+ (SELECT count(*) FROM {links} WHERE kind_id = t.id) uses
			FROM {tags} t LEFT JOIN {tags} a ON a.id = t.alias_of
			WHERE t.name LIKE ? ESCAPE '\\' ORDER BY t.kind DESC, uses DESC, t.name LIMIT ?`,
			like,
			limit
		).map((r) => ({
			id: r.id,
			name: r.name,
			kind: r.kind,
			adult: !!r.adult,
			banned: !!r.banned,
			aliasOf: r.target,
			uses: r.uses
		}));
	}

	private mustFind(name: string): TagRow {
		const row = this.get<TagRow>('SELECT * FROM {tags} WHERE name = ?', this.normalize(name));
		if (!row) throw new TagGraphError(`No tag called “${this.normalize(name)}”.`);
		return row;
	}

	setAdult(name: string, on: boolean, mod: Actor) {
		const tag = this.mustFind(name);
		this.db.transaction(() => {
			this.run('UPDATE {tags} SET adult = ? WHERE id = ? OR alias_of = ?', on ? 1 : 0, tag.id, tag.id);
			this.log(mod.id, on ? 'mark-adult' : 'unmark-adult', null, tag.name);
		})();
		this.changed(null);
	}

	/** Banning takes the tag off every item and out of every link, and refuses it from then on. */
	setBanned(name: string, on: boolean, mod: Actor) {
		const tag = this.mustFind(name);
		if (on && tag.kind === 'link' && tag.name === this.anyLink) throw new TagGraphError(`“${tag.name}” is the word for any link and can't be banned.`);
		this.db.transaction(() => {
			this.run('UPDATE {tags} SET banned = ? WHERE id = ?', on ? 1 : 0, tag.id);
			if (on) {
				this.run('DELETE FROM {item_tags} WHERE tag_id = ?', tag.id);
				this.run('DELETE FROM {links} WHERE kind_id = ? OR id IN (SELECT link_id FROM {link_members} WHERE tag_id = ?)', tag.id, tag.id);
			}
			this.log(mod.id, on ? 'ban' : 'unban', null, tag.name);
		})();
		this.changed(null);
	}

	rename(name: string, to: string, mod: Actor) {
		const tag = this.mustFind(name);
		const next = this.normalize(to);
		const error = nameError(next, this.namespaces);
		if (error) throw new TagGraphError(error);
		if (next === tag.name) return;
		if (tag.kind === 'link' && next.includes(' ')) throw new TagGraphError('Link names are one word, so searches can spot them.');
		if (tag.kind === 'link' && tag.name === this.anyLink) throw new TagGraphError(`“${tag.name}” is the word for any link and can't be renamed.`);
		if (tag.alias_of) throw new TagGraphError(`“${tag.name}” is an alias. Rename the tag it points to instead.`);
		// Renaming back to an old name takes that alias over.
		this.run('DELETE FROM {tags} WHERE name = ? AND alias_of = ?', next, tag.id);
		if (this.get('SELECT 1 FROM {tags} WHERE name = ?', next)) throw new TagGraphError(`“${next}” already exists. Merge into it instead.`);
		this.db.transaction(() => {
			this.run('UPDATE {tags} SET name = ? WHERE id = ?', next, tag.id);
			// The old name stays as an alias, so searches and typing it still work.
			this.run('INSERT INTO {tags} (name, kind, alias_of, created_by, created_at) VALUES (?, ?, ?, ?, ?)', tag.name, tag.kind, tag.id, mod.id, Date.now());
			this.log(mod.id, 'rename', null, `${tag.name} → ${next}`);
		})();
		this.changed(null);
	}

	/**
	 * Moves every use of one tag onto another and keeps the old name as an
	 * alias, so typing it still works. Links that become the same are joined.
	 */
	merge(from: string, into: string, mod: Actor) {
		const a = this.mustFind(from);
		const b = this.find(into) ?? this.mustFind(into);
		if (a.id === b.id) throw new TagGraphError('Those are already the same tag.');
		if (a.kind !== b.kind) throw new TagGraphError("A tag and a link name can't be merged.");
		if (a.kind === 'link' && a.name === this.anyLink) throw new TagGraphError(`“${a.name}” is the word for any link and can't be merged away.`);
		this.db.transaction(() => {
			this.run(
				'INSERT OR IGNORE INTO {item_tags} (item, tag_id, actor, vote, at) SELECT item, ?, actor, vote, at FROM {item_tags} WHERE tag_id = ?',
				b.id,
				a.id
			);
			this.run('DELETE FROM {item_tags} WHERE tag_id = ?', a.id);
			if (a.kind === 'link') {
				for (const l of this.all<{ id: number; item: string; key: string }>('SELECT id, item, key FROM {links} WHERE kind_id = ?', a.id)) {
					const twin = this.get<{ id: number }>('SELECT id FROM {links} WHERE item = ? AND kind_id = ? AND key = ?', l.item, b.id, l.key);
					if (twin) this.fold(l.id, twin.id);
					else this.run('UPDATE {links} SET kind_id = ? WHERE id = ?', b.id, l.id);
				}
			} else {
				const affected = this.all<{ link_id: number }>('SELECT DISTINCT link_id FROM {link_members} WHERE tag_id = ?', a.id);
				this.run('UPDATE {link_members} SET tag_id = ? WHERE tag_id = ?', b.id, a.id);
				for (const { link_id } of affected) this.rekey(link_id);
			}
			this.run('UPDATE {tags} SET alias_of = ? WHERE id = ? OR alias_of = ?', b.id, a.id, a.id);
			this.log(mod.id, 'merge', null, `${a.name} → ${b.name}`);
		})();
		this.changed(null);
	}

	/** After a merge: drop repeated members, and fold the link into an identical one. */
	private rekey(id: number) {
		const link = this.get<{ item: string; kind_id: number }>('SELECT item, kind_id FROM {links} WHERE id = ?', id);
		if (!link) return;
		const members = this.all<{ pos: number; tag_id: number }>('SELECT pos, tag_id FROM {link_members} WHERE link_id = ? ORDER BY pos', id);
		const seen = new Set<number>();
		for (const m of members) {
			if (seen.has(m.tag_id)) this.run('DELETE FROM {link_members} WHERE link_id = ? AND pos = ?', id, m.pos);
			seen.add(m.tag_id);
		}
		if (seen.size < 2) {
			this.run('DELETE FROM {links} WHERE id = ?', id);
			return;
		}
		const key = [...seen].sort((x, y) => x - y).join(',');
		const twin = this.get<{ id: number }>('SELECT id FROM {links} WHERE item = ? AND kind_id = ? AND key = ? AND id != ?', link.item, link.kind_id, key, id);
		if (twin) this.fold(id, twin.id);
		else this.run('UPDATE {links} SET key = ? WHERE id = ?', key, id);
	}

	/** Moves one link's votes onto its twin and deletes it. */
	private fold(id: number, into: number) {
		this.run('INSERT OR IGNORE INTO {link_votes} (link_id, actor, vote, at) SELECT ?, actor, vote, at FROM {link_votes} WHERE link_id = ?', into, id);
		this.run('DELETE FROM {links} WHERE id = ?', id);
	}

	addLinkName(name: string, mod: Actor) {
		const n = this.normalize(name);
		const error = nameError(n, this.namespaces);
		if (error) throw new TagGraphError(error);
		if (n.includes(' ')) throw new TagGraphError('Link names are one word, so searches can spot them.');
		if (this.find(n)) throw new TagGraphError(`“${n}” is already a tag or link name.`);
		this.db.transaction(() => {
			this.run("INSERT INTO {tags} (name, kind, created_by, created_at) VALUES (?, 'link', ?, ?)", n, mod.id, Date.now());
			this.log(mod.id, 'new-link-name', null, n);
		})();
		this.changed(null);
	}

	isBlocked(actor: string) {
		return !!this.get('SELECT 1 FROM {blocked} WHERE actor = ?', actor);
	}

	setBlocked(actor: string, on: boolean, mod: Actor) {
		this.db.transaction(() => {
			if (on) this.run('INSERT OR IGNORE INTO {blocked} (actor, at) VALUES (?, ?)', actor, Date.now());
			else this.run('DELETE FROM {blocked} WHERE actor = ?', actor);
			this.log(mod.id, on ? 'block' : 'unblock', null, actor);
		})();
	}

	/**
	 * Takes back every vote an actor made, dropping links nobody else backs.
	 * Pass `mod` when a moderator does it; without, it's the account going away
	 * and their name is also cleared from the log.
	 */
	forget(actor: string, mod?: Actor) {
		this.db.transaction(() => {
			this.run('DELETE FROM {item_tags} WHERE actor = ?', actor);
			this.run('DELETE FROM {link_votes} WHERE actor = ?', actor);
			this.run('DELETE FROM {links} WHERE NOT EXISTS (SELECT 1 FROM {link_votes} v WHERE v.link_id = {links}.id)');
			if (mod) this.log(mod.id, 'undo-all', null, actor);
			else {
				this.run('UPDATE {log} SET actor = NULL WHERE actor = ?', actor);
				this.run("UPDATE {tags} SET created_by = NULL WHERE created_by = ?", actor);
			}
		})();
		this.changed(null);
	}

	recent(limit = 50, actor?: string): LogEntry[] {
		return actor
			? this.all<LogEntry>('SELECT * FROM {log} WHERE actor = ? ORDER BY id DESC LIMIT ?', actor, limit)
			: this.all<LogEntry>('SELECT * FROM {log} ORDER BY id DESC LIMIT ?', limit);
	}

	stats() {
		const n = (text: string) => this.get<{ n: number }>(text)!.n;
		return {
			tags: n("SELECT count(*) n FROM {tags} WHERE kind = 'tag' AND banned = 0 AND alias_of IS NULL"),
			linkNames: n("SELECT count(*) n FROM {tags} WHERE kind = 'link' AND banned = 0 AND alias_of IS NULL"),
			taggedItems: n('SELECT count(DISTINCT item) n FROM {item_tags}'),
			links: n('SELECT count(*) n FROM {links}'),
			blocked: n('SELECT count(*) n FROM {blocked}')
		};
	}
}

