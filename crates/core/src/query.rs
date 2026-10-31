// The search language. It's booru syntax with real boolean logic on top:
//
//   fox wolf              both tags (AND)
//   -sketch               not this tag
//   fox | wolf            either
//   ~fox ~wolf            either, booru style (one OR group per AND level)
//   (fox | wolf) -sketch  groups, nested as deep as you like
//   fox*                  wildcard
//   score:>100 rating:s   metatags (see META_KEYS)
//   [eat]                 posts where `eat` is in a link with other tags
//   relation[rabbit eat]  a `relation` link holding both rabbit and eat
//
// A ( at the start of a word opens a group. Parentheses later in a word stay
// part of the tag when they balance, so `rem_(re:zero)` is one tag and
// `(rem_(re:zero) | emilia)` still groups.

use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq)]
pub enum Expr {
	/// A tag, maybe with `*` wildcards.
	Tag(String),
	Meta { key: String, value: String },
	/// Tags linked together on a post; `kind: None` means any link name.
	Link { kind: Option<String>, tags: Vec<String> },
	Not(Box<Expr>),
	And(Vec<Expr>),
	Or(Vec<Expr>),
}

/// Metatags, so `rating:s` isn't read as a tag. Any other colon is part of a
/// tag (`re:zero`).
pub const META_KEYS: &[&str] = &[
	"rating", "score", "id", "favcount", "width", "height", "type", "md5", "order", "status", "user", "fav", "pool", "set", "source", "date",
	"ratio", "filesize", "tagcount", "comment_count", "description", "approver", "parent", "child",
];

/// Metatags we can check on a post ourselves. The rest only work when sent to the site.
pub const LOCAL_META_KEYS: &[&str] = &["rating", "score", "id", "favcount", "width", "height", "type", "md5", "order", "favorited"];

/// Metatags only this app knows (never sent to a site): `favorited:yes` is
/// your own favorites, kept in the vault.
pub const APP_META_KEYS: &[&str] = &["favorited"];

#[derive(Debug, Clone, PartialEq)]
enum Tok {
	Word(String),
	Minus,
	Tilde,
	Or,
	Open,
	Close,
	LinkOpen(Option<String>),
	LinkClose,
}

/// A word that ends a link: the tag before `]`, and how many `)` follow it,
/// as in `carrot])`.
fn close_link(raw: &str) -> Option<(&str, usize)> {
	let trimmed = raw.trim_end_matches(')');
	let tag = trimmed.strip_suffix(']')?;
	Some((tag, raw.len() - trimmed.len()))
}

fn lex(text: &str) -> Result<Vec<Tok>> {
	let mut out = Vec::new();
	let mut in_link = false;
	for raw in text.split_whitespace() {
		if in_link {
			match close_link(raw) {
				Some((w, closes)) => {
					if !w.is_empty() {
						out.push(Tok::Word(w.to_string()));
					}
					out.push(Tok::LinkClose);
					out.extend(std::iter::repeat_n(Tok::Close, closes));
					in_link = false;
				}
				None => out.push(Tok::Word(raw.to_string())),
			}
			continue;
		}
		match raw {
			"|" | "||" => {
				out.push(Tok::Or);
				continue;
			}
			"&" | "&&" => continue,
			_ => {}
		}
		if raw.contains(']') && !raw.contains('[') {
			return Err(Error::Query("There's a ] without a matching [.".into()));
		}
		let mut w = raw;
		loop {
			if w.len() > 1 && w.starts_with('-') {
				out.push(Tok::Minus);
			} else if w.len() > 1 && w.starts_with('~') {
				out.push(Tok::Tilde);
			} else if w.starts_with('(') {
				out.push(Tok::Open);
			} else {
				break;
			}
			w = &w[1..];
		}
		// `kind[` or `[` opens a link.
		if let Some(open) = w.find('[')
			&& w[..open].chars().all(|c| c.is_alphanumeric() || c == '_')
		{
			let kind = &w[..open];
			out.push(Tok::LinkOpen((!kind.is_empty()).then(|| kind.to_lowercase())));
			let rest = &w[open + 1..];
			match close_link(rest) {
				Some((tag, closes)) => {
					if !tag.is_empty() {
						out.push(Tok::Word(tag.to_string()));
					}
					out.push(Tok::LinkClose);
					out.extend(std::iter::repeat_n(Tok::Close, closes));
				}
				None => {
					if !rest.is_empty() {
						out.push(Tok::Word(rest.to_string()));
					}
					in_link = true;
				}
			}
			continue;
		}
		// Peel closing parens that don't belong to the tag itself.
		let mut closes = 0;
		while w.ends_with(')') && w.matches(')').count() > w.matches('(').count() {
			w = &w[..w.len() - 1];
			closes += 1;
		}
		if !w.is_empty() {
			out.push(Tok::Word(w.to_string()));
		}
		out.extend(std::iter::repeat_n(Tok::Close, closes));
	}
	if in_link {
		return Err(Error::Query("A link search is missing its closing ]".into()));
	}
	Ok(out)
}

struct Parser {
	toks: Vec<Tok>,
	pos: usize,
}

impl Parser {
	fn peek(&self) -> Option<&Tok> {
		self.toks.get(self.pos)
	}

	fn next(&mut self) -> Option<Tok> {
		let t = self.toks.get(self.pos).cloned();
		self.pos += 1;
		t
	}

	fn or_expr(&mut self) -> Result<Expr> {
		let mut items = vec![self.and_expr()?];
		while self.peek() == Some(&Tok::Or) {
			self.pos += 1;
			items.push(self.and_expr()?);
		}
		Ok(if items.len() == 1 { items.pop().unwrap() } else { Expr::Or(items) })
	}

	fn and_expr(&mut self) -> Result<Expr> {
		let mut items = Vec::new();
		let mut tilde = Vec::new();
		while let Some(t) = self.peek() {
			if matches!(t, Tok::Or | Tok::Close) {
				break;
			}
			if *t == Tok::Tilde {
				self.pos += 1;
				tilde.push(self.unary()?);
			} else {
				items.push(self.unary()?);
			}
		}
		match tilde.len() {
			0 => {}
			1 => items.push(tilde.pop().unwrap()),
			_ => items.push(Expr::Or(tilde)),
		}
		match items.len() {
			0 => Err(Error::Query(if self.pos >= self.toks.len() && self.pos > 0 {
				"The search ends where a tag should be.".into()
			} else {
				"Empty group or `|` with nothing on one side.".into()
			})),
			1 => Ok(items.pop().unwrap()),
			_ => Ok(Expr::And(items)),
		}
	}

	fn unary(&mut self) -> Result<Expr> {
		match self.next() {
			Some(Tok::Minus) => Ok(Expr::Not(Box::new(self.unary()?))),
			Some(Tok::Tilde) => self.unary(),
			Some(Tok::Open) => {
				let e = self.or_expr()?;
				match self.next() {
					Some(Tok::Close) => Ok(e),
					_ => Err(Error::Query("A ( is never closed.".into())),
				}
			}
			Some(Tok::LinkOpen(kind)) => {
				let mut tags = Vec::new();
				loop {
					match self.next() {
						Some(Tok::Word(w)) => tags.push(normalize_tag(&w)),
						Some(Tok::LinkClose) => break,
						_ => return Err(Error::Query("Links hold plain tags only.".into())),
					}
				}
				if tags.is_empty() {
					return Err(Error::Query("Put at least one tag inside [ ].".into()));
				}
				Ok(Expr::Link { kind, tags })
			}
			Some(Tok::Word(w)) => Ok(word(&w)),
			Some(Tok::Close) => Err(Error::Query("There's a ) without a matching (.".into())),
			Some(Tok::Or) => Err(Error::Query("`|` needs something on both sides.".into())),
			Some(Tok::LinkClose) => Err(Error::Query("There's a ] without a matching [.".into())),
			None => Err(Error::Query("The search ends where a tag should be.".into())),
		}
	}
}

fn word(w: &str) -> Expr {
	if let Some((k, v)) = w.split_once(':') {
		let key = k.to_lowercase();
		if (META_KEYS.contains(&key.as_str()) || APP_META_KEYS.contains(&key.as_str())) && !v.is_empty() {
			return Expr::Meta { key, value: v.to_lowercase() };
		}
	}
	Expr::Tag(normalize_tag(w))
}

/// Booru form: lowercase, no spaces.
pub fn normalize_tag(t: &str) -> String {
	t.trim().to_lowercase()
}

/// Reads a search. Empty text is an empty AND, which matches everything.
pub fn parse(text: &str) -> Result<Expr> {
	let toks = lex(text)?;
	if toks.is_empty() {
		return Ok(Expr::And(vec![]));
	}
	let mut p = Parser { toks, pos: 0 };
	let e = p.or_expr()?;
	match p.peek() {
		None => Ok(e),
		Some(Tok::Close) => Err(Error::Query("There's a ) without a matching (.".into())),
		Some(_) => Err(Error::Query("Couldn't read the end of the search.".into())),
	}
}

/// What a search is checked against, so the evaluator needn't know about posts or the graph.
pub trait Subject {
	fn has_tag(&self, tag: &str) -> bool;
	/// Whether a metatag matches, or None when it can't be checked here.
	fn meta(&self, key: &str, value: &str) -> Option<bool>;
	fn linked(&self, kind: Option<&str>, tags: &[String]) -> bool;
	/// Every tag, for wildcards.
	fn tags(&self) -> Box<dyn Iterator<Item = &str> + '_>;
}

impl Expr {
	/// Whether a post matches. Metatags we can't check count as matching:
	/// the site checked them.
	pub fn matches(&self, s: &dyn Subject) -> bool {
		match self {
			Expr::Tag(t) if t.contains('*') => s.tags().any(|have| glob(t, have)),
			Expr::Tag(t) => s.has_tag(t),
			Expr::Meta { key, value } => s.meta(key, value).unwrap_or(true),
			Expr::Link { kind, tags } => s.linked(kind.as_deref(), tags),
			// An unknown metatag inside a NOT can't be checked either way.
			Expr::Not(e) => match e.as_ref() {
				Expr::Meta { key, value } => s.meta(key, value).is_none_or(|m| !m),
				e => !e.matches(s),
			},
			Expr::And(es) => es.iter().all(|e| e.matches(s)),
			Expr::Or(es) => es.iter().any(|e| e.matches(s)),
		}
	}

	pub fn walk(&self, f: &mut dyn FnMut(&Expr)) {
		f(self);
		match self {
			Expr::Not(e) => e.walk(f),
			Expr::And(es) | Expr::Or(es) => es.iter().for_each(|e| e.walk(f)),
			_ => {}
		}
	}

	pub fn any(&self, pred: &dyn Fn(&Expr) -> bool) -> bool {
		let mut found = false;
		self.walk(&mut |e| found |= pred(e));
		found
	}

	/// The value of the first metatag with this key, e.g. `order`.
	pub fn meta_value(&self, key: &str) -> Option<String> {
		let mut out = None;
		self.walk(&mut |e| {
			if let Expr::Meta { key: k, value } = e
				&& k == key
				&& out.is_none()
			{
				out = Some(value.clone());
			}
		});
		out
	}
}

impl std::fmt::Display for Expr {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Expr::Tag(t) => write!(f, "{t}"),
			Expr::Meta { key, value } => write!(f, "{key}:{value}"),
			Expr::Link { kind, tags } => write!(f, "{}[{}]", kind.as_deref().unwrap_or(""), tags.join(" ")),
			Expr::Not(e) => match e.as_ref() {
				Expr::And(_) | Expr::Or(_) => write!(f, "-({e})"),
				e => write!(f, "-{e}"),
			},
			Expr::And(es) => {
				let parts: Vec<String> = es.iter().map(|e| if matches!(e, Expr::Or(_)) { format!("({e})") } else { e.to_string() }).collect();
				write!(f, "{}", parts.join(" "))
			}
			Expr::Or(es) => {
				let parts: Vec<String> = es.iter().map(|e| if matches!(e, Expr::And(_)) { format!("({e})") } else { e.to_string() }).collect();
				write!(f, "{}", parts.join(" | "))
			}
		}
	}
}

/// `*` matches any run of characters.
pub fn glob(pattern: &str, s: &str) -> bool {
	let parts: Vec<&str> = pattern.split('*').collect();
	if parts.len() == 1 {
		return pattern == s;
	}
	let (first, last) = (parts[0], parts[parts.len() - 1]);
	if !s.starts_with(first) || !s[first.len()..].ends_with(last) || s.len() < first.len() + last.len() {
		return false;
	}
	let mut rest = &s[first.len()..s.len() - last.len()];
	for mid in &parts[1..parts.len() - 1] {
		match rest.find(mid) {
			Some(i) => rest = &rest[i + mid.len()..],
			None => return false,
		}
	}
	true
}

/// Compares a number against `5`, `>5`, `>=5`, `<5`, `<=5` or `5..10`.
pub fn compare(n: i64, spec: &str) -> Option<bool> {
	let num = |s: &str| s.trim().parse::<i64>().ok();
	if let Some((a, b)) = spec.split_once("..") {
		let lo = if a.is_empty() { i64::MIN } else { num(a)? };
		let hi = if b.is_empty() { i64::MAX } else { num(b)? };
		return Some(n >= lo && n <= hi);
	}
	Some(if let Some(v) = spec.strip_prefix(">=") {
		n >= num(v)?
	} else if let Some(v) = spec.strip_prefix("<=") {
		n <= num(v)?
	} else if let Some(v) = spec.strip_prefix('>') {
		n > num(v)?
	} else if let Some(v) = spec.strip_prefix('<') {
		n < num(v)?
	} else {
		n == num(spec)?
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn p(s: &str) -> String {
		parse(s).unwrap().to_string()
	}

	#[test]
	fn plain_and_negation() {
		assert_eq!(parse("fox -sketch").unwrap(), Expr::And(vec![Expr::Tag("fox".into()), Expr::Not(Box::new(Expr::Tag("sketch".into())))]));
	}

	#[test]
	fn precedence_and_groups() {
		assert_eq!(p("a b | c"), "(a b) | c");
		assert_eq!(p("a (b | c)"), "a (b | c)");
		assert_eq!(p("-(a | b) c"), "-(a | b) c");
		assert_eq!(p("((a))"), "a");
	}

	#[test]
	fn tilde_groups() {
		assert_eq!(p("~fox ~wolf solo"), "solo (fox | wolf)");
		assert_eq!(p("~fox solo"), "solo fox");
	}

	#[test]
	fn parens_inside_tags() {
		assert_eq!(parse("rem_(re:zero)").unwrap(), Expr::Tag("rem_(re:zero)".into()));
		assert_eq!(p("(rem_(re:zero) | emilia_(re:zero))"), "rem_(re:zero) | emilia_(re:zero)");
	}

	#[test]
	fn metas_and_colons() {
		assert_eq!(parse("score:>10").unwrap(), Expr::Meta { key: "score".into(), value: ">10".into() });
		assert_eq!(parse("re:zero").unwrap(), Expr::Tag("re:zero".into()));
	}

	#[test]
	fn links() {
		assert_eq!(parse("[eat]").unwrap(), Expr::Link { kind: None, tags: vec!["eat".into()] });
		assert_eq!(parse("relation[rabbit eat] solo").unwrap().to_string(), "relation[rabbit eat] solo");
		assert_eq!(parse("relation[ rabbit eat ]").unwrap(), Expr::Link { kind: Some("relation".into()), tags: vec!["rabbit".into(), "eat".into()] });
	}

	#[test]
	fn errors() {
		for bad in ["(a", "a)", "a |", "| a", "[a", "a ]", "()", "[]"] {
			assert!(parse(bad).is_err(), "{bad} should fail");
		}
		assert_eq!(parse("  ").unwrap(), Expr::And(vec![]));
	}

	#[test]
	fn globbing() {
		assert!(glob("fox*", "fox_girl"));
		assert!(glob("*_girl", "fox_girl"));
		assert!(glob("f*x*l", "fox_girl"));
		assert!(!glob("fox*", "wolf"));
		assert!(!glob("ab*ba", "aba"));
	}

	#[test]
	fn comparing() {
		assert_eq!(compare(5, ">4"), Some(true));
		assert_eq!(compare(5, "<=4"), Some(false));
		assert_eq!(compare(5, "1..5"), Some(true));
		assert_eq!(compare(5, "6.."), Some(false));
		assert_eq!(compare(5, "x"), None);
	}

	#[test]
	fn links_inside_groups() {
		let link = |a: &str, b: &str| Expr::Link { kind: Some("eat".into()), tags: vec![a.into(), b.into()] };
		assert_eq!(
			parse("(eat[bone dog] | eat[carrot rabbit]) fox").unwrap(),
			Expr::And(vec![Expr::Or(vec![link("bone", "dog"), link("carrot", "rabbit")]), Expr::Tag("fox".into())])
		);
		assert_eq!(parse("(eat[bone dog])").unwrap(), link("bone", "dog"));
	}

}
