// How tag names are written and compared. Shared by the graph and its query parser.

export const MAX_NAME = 40;

/**
 * One way to write each name: NFKC, lowercase, single spaces. A known
 * namespace keeps its colon ("Character: Rem" → "character:rem"); any other
 * colon is part of the name ("re:zero").
 */
export function normalizeName(raw: string, namespaces: readonly string[]): string {
	const s = raw.normalize('NFKC').toLowerCase().replace(/[\s_]+/g, ' ').trim();
	const colon = s.indexOf(':');
	if (colon > 0) {
		const ns = s.slice(0, colon).trim();
		if (namespaces.includes(ns)) return `${ns}:${s.slice(colon + 1).trim()}`;
	}
	return s;
}

/** Why a normalized name can't be used, or null when it's fine. */
export function nameError(name: string, namespaces: readonly string[]): string | null {
	const bare = namespaceOf(name, namespaces) ? name.slice(name.indexOf(':') + 1) : name;
	if (!bare) return 'Type a tag name.';
	if (name.length > MAX_NAME) return `Tags can be at most ${MAX_NAME} characters.`;
	if (!/^[\p{L}\p{N}][\p{L}\p{N} '\-.!?&+:]*$/u.test(name)) return 'Tags can use letters, numbers, spaces and - \' . ! ? & +';
	if (name.includes(',')) return 'One tag at a time.';
	return null;
}

export function namespaceOf(name: string, namespaces: readonly string[]): string {
	const colon = name.indexOf(':');
	if (colon <= 0) return '';
	const ns = name.slice(0, colon);
	return namespaces.includes(ns) ? ns : '';
}

/** Words of a name or a search, for matching: letters and numbers only. */
export const words = (s: string) =>
	s
		.normalize('NFKD')
		.replace(/\p{M}/gu, '')
		.toLowerCase()
		.split(/[^\p{L}\p{N}:']+/u)
		.filter(Boolean);
