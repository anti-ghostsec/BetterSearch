# taggraph

Community tags, votes and **links** for any app, in SQLite. It only needs
`better-sqlite3`: copy this folder into another project and use it as is.

- **Items** are opaque string keys you choose: `anilist:21`, `booru:post/991`.
- **Actors** are string ids for whoever makes a change (your user ids).
- **Tags** go on items and are voted up or down; a tag shows while its score is above zero.
- **Links** are named groups of tags already on one item, in order:
  `relation: rabbit · eat · carrot`. Link names (`link`, `relation`, …) are
  made by moderators. The first link name (`link` by default) means "any link"
  in searches.
- **Searching** `eat link` finds every item where `eat` is linked to
  something; `rabbit eat relation` only `relation` links holding both.

## Files

- `graph.ts`: the `TagGraph` class (schema, tagging, links, search, moderation)
- `names.ts`: how names are normalized and checked
- `index.ts`: what to import

## Use

```ts
import Database from 'better-sqlite3';
import { TagGraph, TagGraphError } from './taggraph';

const db = new Database('app.db');
db.pragma('foreign_keys = ON');
const graph = new TagGraph(db, {
	prefix: 'tg', // table prefix, so it can share a database
	namespaces: ['character', 'meta'], // allowed "namespace:name" prefixes
	linkNames: ['link', 'relation'], // created on first run; the first means "any"
	limits: { perHour: 30, newPerDay: 10, newSitePerHour: 60, maxLinkSize: 5 }
});

const alice = { id: '1', adult: false, canCreate: true };
graph.voteTag('post:991', 'cow', alice, 1);
graph.voteTag('post:991', 'drink', alice, 1);
graph.voteTag('post:991', 'milk', alice, 1);
graph.addLink('post:991', 'relation', ['cow', 'drink', 'milk'], alice);

const q = graph.parse('drink link', false); // { kinds: [], tags: ['drink'], rest: '' }
if (q) graph.findLinked(q, { adult: false }); // Map { 'post:991' => [{ kind: 'relation', members: [...] }] }
```

Refusals (bad names, rate limits, blocked accounts) throw `TagGraphError`
with a message meant for the person.

| Method | What it does |
| --- | --- |
| `voteTag(item, name, actor, 1 \| -1 \| 0, adult?)` | Tag, vote down, or take back a vote; voting a new name on creates it |
| `itemTags(item, viewer)` / `itemLinks(item, viewer)` | What to show on an item's page |
| `addLink(item, kind, tags, actor, onItem?)` | Link tags on the item; `onItem` adds tags your app owns (e.g. a catalog's) |
| `voteLink(id, actor, vote)` | Vote on a link |
| `parse(text, adult)` / `findLinked(query, viewer)` | Link searches |
| `visibleTags(item?)` / `popular(n, adult)` / `linkNames(adult)` | For search indexes and suggestions |
| `onChange(fn)` | Called with changed item keys (or null for "maybe all") |
| `rename`, `merge`, `setAdult`, `setBanned`, `addLinkName` | Moderation; old names stay as aliases |
| `removeItemTag`, `removeLink`, `setBlocked`, `forget`, `recent`, `stats` | More moderation and the activity log |

Hosts decide policy through the actor: `canCreate` (e.g. accounts older than a
day), `adult`, `moderator` (skips the hourly limit and may moderate).
