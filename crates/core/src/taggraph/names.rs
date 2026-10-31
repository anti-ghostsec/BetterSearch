// How tag names are written and compared. Ported from the site's names.ts, but
// in booru form: spaces become underscores, since booru tags can't hold spaces.

use unicode_normalization::UnicodeNormalization;

pub const MAX_NAME: usize = 100;

/// One way to write each name: NFKC, lowercase, underscores between words. A
/// known namespace keeps its colon ("Character: Rem" → "character:rem"); any
/// other colon is part of the name ("re:zero").
pub fn normalize_name(raw: &str, namespaces: &[String]) -> String {
	let s: String = raw.nfkc().collect::<String>().to_lowercase();
	let s = s.split_whitespace().collect::<Vec<_>>().join("_");
	if let Some(colon) = s.find(':')
		&& colon > 0
	{
		let ns = s[..colon].trim_matches('_');
		if namespaces.iter().any(|n| n == ns) {
			return format!("{ns}:{}", s[colon + 1..].trim_matches('_'));
		}
	}
	s
}

/// Why a normalized name can't be used, or None when it's fine.
pub fn name_error(name: &str, namespaces: &[String]) -> Option<String> {
	let bare = if namespace_of(name, namespaces).is_empty() { name } else { &name[name.find(':').unwrap() + 1..] };
	if bare.is_empty() {
		return Some("Type a tag name.".into());
	}
	if name.chars().count() > MAX_NAME {
		return Some(format!("Tags can be at most {MAX_NAME} characters."));
	}
	if name.starts_with(['-', '~', '(', ')', '*', '@']) {
		return Some("Tags can't start with - ~ ( ) * or @ (that starts a group in searches).".into());
	}
	if name.contains([',', '[', ']', '|']) {
		return Some("Tags can't hold , [ ] or |.".into());
	}
	if let Some((k, _)) = name.split_once(':')
		&& crate::query::META_KEYS.contains(&k)
	{
		return Some(format!("“{k}:” is a search word, so it can't start a tag."));
	}
	None
}

pub fn namespace_of<'a>(name: &str, namespaces: &'a [String]) -> &'a str {
	match name.find(':') {
		Some(colon) if colon > 0 => namespaces.iter().find(|n| **n == name[..colon]).map_or("", |n| n.as_str()),
		_ => "",
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn normalizes() {
		let ns = vec!["character".to_string()];
		assert_eq!(normalize_name("  Long   Hair ", &ns), "long_hair");
		assert_eq!(normalize_name("Character: Rem", &ns), "character:rem");
		assert_eq!(normalize_name("Re:Zero", &ns), "re:zero");
		assert_eq!(normalize_name("ｆｏｘ", &ns), "fox");
	}

	#[test]
	fn rejects() {
		let ns = vec!["character".to_string()];
		assert!(name_error("", &ns).is_some());
		assert!(name_error("character:", &ns).is_some());
		assert!(name_error("-fox", &ns).is_some());
		assert!(name_error("a,b", &ns).is_some());
		assert!(name_error("rating:s", &ns).is_some());
		assert!(name_error("rem_(re:zero)", &ns).is_none());
	}
}
