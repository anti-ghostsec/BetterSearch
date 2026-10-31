// A reusable tag graph: community tags on opaque item keys, votes, links
// between tags on an item, link searches and moderation. Depends only on
// better-sqlite3, so it can move to its own package or service unchanged.

export { TagGraph, TagGraphError } from './graph';
export type { Actor, ItemLink, ItemTag, Limits, LinkMatch, LinkQuery, LogEntry, TagGraphOptions, TagInfo, Viewer, Vote } from './graph';
export { MAX_NAME, normalizeName } from './names';
