//! The local API other programs search through.
//!
//! Nothing here opens a socket: [`handle`] takes a parsed [`Request`] and
//! returns a [`Response`], so the routing, auth, limits and read-only gating
//! are all testable without one. The app owns the listener and implements
//! [`Backend`].
//!
//! Shaped for another program (often another AI) rather than a person: every
//! answer is JSON, `/openapi.json` describes the rest, and `/file` hands over
//! the bytes so the caller never has to know a site's hotlink rules or keys.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::Rating;

pub const MAX_HEADER_BYTES: usize = 8 * 1024;
pub const MAX_BODY_BYTES: usize = 64 * 1024;
pub const MAX_QUERY_CHARS: usize = 512;
pub const MAX_RESULTS: usize = 100;
pub const MAX_SITES: usize = 6;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
	Get,
	Post,
}

#[derive(Debug, Clone)]
pub struct Request {
	pub method: Method,
	pub path: String,
	pub query: HashMap<String, String>,
	pub headers: Vec<(String, String)>,
	pub body: Vec<u8>,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum ParseError {
	#[error("the request line is malformed")]
	Line,
	#[error("the request headers are too big")]
	Headers,
	#[error("the body is too big")]
	Body,
	#[error("unsupported method")]
	Method,
}

impl Request {
	pub fn header(&self, name: &str) -> Option<&str> {
		self.headers.iter().find(|(n, _)| n.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
	}

	fn param(&self, name: &str) -> Option<&str> {
		self.query.get(name).map(String::as_str)
	}

	/// A query parameter as a number, clamped.
	fn number(&self, name: &str, default: usize, lo: usize, hi: usize) -> usize {
		self.param(name).and_then(|v| v.parse().ok()).unwrap_or(default).clamp(lo, hi)
	}
}

/// Splits the head off a request: the first line, then headers. Returns the
/// head, the rest of the buffer, and how many bytes the head took.
fn head(raw: &[u8]) -> Result<(&str, &[u8], usize), ParseError> {
	let end = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or(ParseError::Line)?;
	if end > MAX_HEADER_BYTES {
		return Err(ParseError::Headers);
	}
	let text = std::str::from_utf8(&raw[..end]).map_err(|_| ParseError::Line)?;
	Ok((text, &raw[end + 4..], end + 4))
}

fn percent_decode(s: &str) -> String {
	let bytes = s.as_bytes();
	let mut out = Vec::with_capacity(bytes.len());
	let mut i = 0;
	while i < bytes.len() {
		match bytes[i] {
			b'%' if i + 2 < bytes.len() => match u8::from_str_radix(&s[i + 1..i + 3], 16) {
				Ok(b) => {
					out.push(b);
					i += 3;
				}
				Err(_) => {
					out.push(b'%');
					i += 1;
				}
			},
			b'+' => {
				out.push(b' ');
				i += 1;
			}
			b => {
				out.push(b);
				i += 1;
			}
		}
	}
	String::from_utf8_lossy(&out).into_owned()
}

fn split_target(target: &str) -> (String, HashMap<String, String>) {
	let (path, query) = target.split_once('?').unwrap_or((target, ""));
	let mut map = HashMap::new();
	for pair in query.split('&').filter(|p| !p.is_empty()) {
		let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
		map.insert(percent_decode(k), percent_decode(v));
	}
	(path.to_string(), map)
}

impl Request {
	/// Parses a whole request. `body` is whatever arrived after the head.
	pub fn parse(raw: &[u8]) -> Result<(Request, usize), ParseError> {
		let (text, rest, used) = head(raw)?;
		let mut lines = text.split("\r\n");
		let line = lines.next().ok_or(ParseError::Line)?;
		let mut parts = line.split(' ');
		let method = match parts.next().ok_or(ParseError::Line)? {
			"GET" => Method::Get,
			"POST" => Method::Post,
			_ => return Err(ParseError::Method),
		};
		let target = parts.next().ok_or(ParseError::Line)?;
		if !target.starts_with('/') {
			return Err(ParseError::Line);
		}
		let (path, query) = split_target(target);
		let mut headers = Vec::new();
		let mut length = 0usize;
		for l in lines {
			let Some((k, v)) = l.split_once(':') else { continue };
			let (k, v) = (k.trim(), v.trim());
			if k.eq_ignore_ascii_case("content-length") {
				length = v.parse().map_err(|_| ParseError::Line)?;
				if length > MAX_BODY_BYTES {
					return Err(ParseError::Body);
				}
			}
			headers.push((k.to_string(), v.to_string()));
		}
		if rest.len() < length {
			return Err(ParseError::Line);
		}
		let body = rest[..length].to_vec();
		Ok((Request { method, path, query, headers, body }, used + length))
	}

	pub fn json(&self) -> Option<Value> {
		serde_json::from_slice(&self.body).ok()
	}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Route {
	Health,
	OpenApi,
	Search,
	Post { site: String, id: u64 },
	File { site: String, id: u64, thumb: bool },
	Taste,
	Write(Write),
	NotFound,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Write {
	Favorite,
	Feedback,
	Regenerate,
}

/// Turns a path into a route. Unknown paths are [`Route::NotFound`], not errors.
pub fn route(method: Method, path: &str) -> Route {
	let path = path.trim_end_matches('/');
	let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
	let id = |s: &&str| s.parse::<u64>().ok();
	match (method, parts.as_slice()) {
		(Method::Get, ["api", "v1", "health"]) => Route::Health,
		(Method::Get, ["api", "v1", "openapi.json"]) => Route::OpenApi,
		(Method::Get, ["api", "v1", "search"]) => Route::Search,
		(Method::Get, ["api", "v1", "taste"]) => Route::Taste,
		(Method::Get, ["api", "v1", "post", site, idstr]) => match id(idstr) {
			Some(id) => Route::Post { site: percent_decode(site), id },
			None => Route::NotFound,
		},
		(Method::Get, ["api", "v1", "post", site, idstr, kind]) => match (id(idstr), *kind) {
			(Some(id), "file") => Route::File { site: percent_decode(site), id, thumb: false },
			(Some(id), "thumb") => Route::File { site: percent_decode(site), id, thumb: true },
			_ => Route::NotFound,
		},
		(Method::Post, ["api", "v1", "favorite"]) => Route::Write(Write::Favorite),
		(Method::Post, ["api", "v1", "feedback"]) => Route::Write(Write::Feedback),
		(Method::Post, ["api", "v1", "taste", "regenerate"]) => Route::Write(Write::Regenerate),
		_ => Route::NotFound,
	}
}

/// Compares without leaking where they differ.
pub fn token_ok(given: &str, wanted: &str) -> bool {
	let (a, b) = (given.as_bytes(), wanted.as_bytes());
	if a.len() != b.len() {
		return false;
	}
	let mut diff = 0u8;
	for (x, y) in a.iter().zip(b) {
		diff |= x ^ y;
	}
	diff == 0
}

/// A sliding minute of hits, so a loop can't hammer the sites.
pub struct Limiter {
	per_minute: u32,
	hits: Mutex<Vec<Instant>>,
}

impl Limiter {
	pub fn new(per_minute: u32) -> Self {
		Self { per_minute: per_minute.max(1), hits: Mutex::new(Vec::new()) }
	}

	pub fn check(&self, now: Instant) -> bool {
		let mut hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
		hits.retain(|t| now.duration_since(*t) < Duration::from_secs(60));
		if hits.len() >= self.per_minute as usize {
			return false;
		}
		hits.push(now);
		true
	}

	pub fn retry_after(&self) -> u64 {
		let hits = self.hits.lock().unwrap_or_else(|e| e.into_inner());
		hits.first().map(|t| 60u64.saturating_sub(t.elapsed().as_secs())).unwrap_or(0).max(1)
	}
}

/// One search as the caller asked for it. `sites` and `ratings` are empty when
/// it left them to the app's settings.
#[derive(Debug, Clone)]
pub struct SearchReq {
	pub text: String,
	pub sites: Vec<String>,
	pub ratings: Vec<Rating>,
	pub limit: usize,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Summary {
	pub site: String,
	pub id: u64,
	pub rating: String,
	pub tags: Vec<String>,
	pub width: u32,
	pub height: u32,
	pub page_url: String,
	/// The app's own proxy links, so the caller needs no site keys.
	pub file: String,
	pub thumb: String,
}

#[derive(Debug)]
pub struct File {
	pub content_type: String,
	pub bytes: Vec<u8>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TasteView {
	pub brief: String,
	pub dial: f32,
	pub intensity: f32,
	pub tags: Vec<String>,
	pub updated_at: i64,
}

/// What the app does for the API.
pub trait Backend: Send + Sync {
	fn search(&self, req: &SearchReq) -> Result<Vec<Summary>, String>;
	fn post(&self, site: &str, id: u64) -> Result<Option<Summary>, String>;
	fn file(&self, site: &str, id: u64, thumb: bool) -> Result<Option<File>, String>;
	fn taste(&self) -> Result<Option<TasteView>, String>;
	/// Favorites, feedback and brief regeneration. Only called when not read-only.
	fn write(&self, _write: Write, _body: Value) -> Result<Value, String> {
		Err("the API is read-only".into())
	}
}

pub struct Context<'a> {
	pub token: &'a str,
	pub read_only: bool,
	pub limiter: &'a Limiter,
	pub backend: &'a dyn Backend,
	pub now: Instant,
}

#[derive(Debug)]
pub struct Response {
	pub status: u16,
	pub content_type: String,
	pub headers: Vec<(&'static str, String)>,
	pub body: Vec<u8>,
}

impl Response {
	fn json(status: u16, body: Value) -> Self {
		Response {
			status,
			content_type: "application/json".into(),
			headers: vec![("Cache-Control", "no-store".into())],
			body: serde_json::to_vec(&body).unwrap_or_default(),
		}
	}

	pub fn error(status: u16, message: &str) -> Self {
		Self::json(status, json!({ "error": message }))
	}

	/// The wire form, headers included.
	pub fn to_bytes(&self) -> Vec<u8> {
		let mut out = format!("HTTP/1.1 {} {}\r\n", self.status, reason(self.status));
		out.push_str(&format!("Content-Type: {}\r\nContent-Length: {}\r\n", self.content_type, self.body.len()));
		for (k, v) in &self.headers {
			out.push_str(&format!("{k}: {v}\r\n"));
		}
		out.push_str("Connection: close\r\n\r\n");
		let mut bytes = out.into_bytes();
		bytes.extend_from_slice(&self.body);
		bytes
	}
}

fn reason(status: u16) -> &'static str {
	match status {
		200 => "OK",
		400 => "Bad Request",
		401 => "Unauthorized",
		404 => "Not Found",
		405 => "Method Not Allowed",
		409 => "Conflict",
		413 => "Payload Too Large",
		429 => "Too Many Requests",
		500 => "Internal Server Error",
		_ => "Error",
	}
}

/// Routes and runs one request.
pub fn handle(req: &Request, cx: &Context<'_>) -> Response {
	let route = route(req.method, &req.path);

	// Health and the schema are open: a caller uses them to find the rest.
	if matches!(&route, Route::Health) {
		return Response::json(200, json!({ "ok": true, "api": "bettersearch", "version": env!("CARGO_PKG_VERSION") }));
	}
	if matches!(&route, Route::OpenApi) {
		return Response::json(200, openapi());
	}

	let given = req.header("authorization").unwrap_or_default();
	let given = given.strip_prefix("Bearer ").or_else(|| given.strip_prefix("bearer ")).unwrap_or(given);
	if !token_ok(given, cx.token) {
		let mut r = Response::error(401, "a bearer token is required");
		r.headers.push(("WWW-Authenticate", "Bearer".into()));
		return r;
	}

	if route == Route::NotFound {
		return Response::error(404, "no such route");
	}

	if matches!(route, Route::Search | Route::Post { .. } | Route::File { .. } | Route::Taste) && !cx.limiter.check(cx.now) {
		let mut r = Response::error(429, "too many requests");
		r.headers.push(("Retry-After", cx.limiter.retry_after().to_string()));
		return r;
	}

	if let Route::Write(write) = &route {
		if cx.read_only {
			return Response::error(409, "the API is read-only; allow writing in Settings");
		}
		let Some(body) = req.json() else { return Response::error(400, "the body must be JSON") };
		return match cx.backend.write(write.clone(), body) {
			Ok(v) => Response::json(200, v),
			Err(e) => Response::error(400, &e),
		};
	}

	match route {
		Route::Search => {
			let Some(q) = req.param("q").filter(|q| !q.trim().is_empty()) else {
				return Response::error(400, "q is required");
			};
			if q.chars().count() > MAX_QUERY_CHARS {
				return Response::error(413, "q is too long");
			}
			let limit = req.number("limit", 40, 1, MAX_RESULTS);
			let sites: Vec<String> = req
				.param("sites")
				.unwrap_or_default()
				.split(',')
				.map(|s| s.trim().to_lowercase())
				.filter(|s| !s.is_empty())
				.take(MAX_SITES)
				.collect();
			let ratings = match req.param("ratings").unwrap_or_default() {
				"" => Vec::new(),
				list => {
					let mut out = Vec::new();
					for c in list.chars().filter(|c| *c != ',') {
						match Rating::from_name(&c.to_string()) {
							Some(r) => out.push(r),
							None => return Response::error(400, &format!("unknown rating {c:?}: use s, q, e")),
						}
					}
					out
				}
			};
			let asked = SearchReq { text: q.to_string(), sites, ratings, limit };
			match cx.backend.search(&asked) {
				Ok(posts) => Response::json(200, json!({
					"query": q,
					"sites": asked.sites,
					"ratings": asked.ratings.iter().map(|r| r.letter().to_string()).collect::<Vec<_>>(),
					"limit": limit,
					"posts": posts,
				})),
				Err(e) => Response::error(400, &e),
			}
		}
		Route::Post { site, id } => match cx.backend.post(&site, id) {
			Ok(Some(post)) => Response::json(200, serde_json::to_value(post).unwrap_or_default()),
			Ok(None) => Response::error(404, "no such post"),
			Err(e) => Response::error(400, &e),
		},
		Route::File { site, id, thumb } => match cx.backend.file(&site, id, thumb) {
			Ok(Some(file)) => Response {
				status: 200,
				content_type: file.content_type,
				headers: vec![("Cache-Control", "no-store".into()), ("X-Content-Type-Options", "nosniff".into())],
				body: file.bytes,
			},
			Ok(None) => Response::error(404, "no such post"),
			Err(e) => Response::error(400, &e),
		},
		Route::Taste => match cx.backend.taste() {
			Ok(Some(t)) => Response::json(200, serde_json::to_value(t).unwrap_or_default()),
			Ok(None) => Response::error(404, "the Taste tab is off"),
			Err(e) => Response::error(400, &e),
		},
		_ => Response::error(404, "no such route"),
	}
}

/// The schema a caller reads to configure itself. Deliberately kept in step with
/// [`route`]; the token and the site keys never appear in it.
pub fn openapi() -> Value {
	let post_ref = json!({ "$ref": "#/components/schemas/Summary" });
	json!({
		"openapi": "3.1.0",
		"info": {
			"title": "BetterSearch",
			"version": env!("CARGO_PKG_VERSION"),
			"description": "Booru search for other programs. Bearer token required. \
				/file and /thumb return the image itself; prefer them over the site's own URLs.",
		},
		"components": {
			"securitySchemes": { "bearer": { "type": "http", "scheme": "bearer" } },
			"schemas": {
				"Summary": {
					"type": "object",
					"properties": {
						"site": { "type": "string" },
						"id": { "type": "integer" },
						"rating": { "type": "string", "enum": ["s", "q", "e"] },
						"tags": { "type": "array", "items": { "type": "string" } },
						"width": { "type": "integer" },
						"height": { "type": "integer" },
						"page_url": { "type": "string" },
						"file": { "type": "string" },
						"thumb": { "type": "string" }
					}
				}
			}
		},
		"security": [{ "bearer": [] }],
		"paths": {
			"/api/v1/health": { "get": { "security": [], "summary": "Liveness and version" } },
			"/api/v1/openapi.json": { "get": { "security": [], "summary": "This schema" } },
			"/api/v1/search": {
				"get": {
					"summary": "Run a search in the app's query language",
					"parameters": [
						{ "name": "q", "in": "query", "required": true, "schema": { "type": "string", "maxLength": MAX_QUERY_CHARS } },
						{ "name": "limit", "in": "query", "schema": { "type": "integer", "minimum": 1, "maximum": MAX_RESULTS, "default": 40 } },
						{ "name": "sites", "in": "query", "description": "Comma-separated site ids; the app's current set if left out", "schema": { "type": "string" } },
						{ "name": "ratings", "in": "query", "description": "s, q and/or e; the app's ticked ratings if left out", "schema": { "type": "string" } }
					],
					"responses": { "200": { "description": "Matches" } }
				}
			},
			"/api/v1/post/{site}/{id}": {
				"get": {
					"summary": "One post",
					"parameters": [
						{ "name": "site", "in": "path", "required": true, "schema": { "type": "string" } },
						{ "name": "id", "in": "path", "required": true, "schema": { "type": "integer" } }
					],
					"responses": { "200": { "description": "The post", "content": { "application/json": { "schema": post_ref } } } }
				}
			},
			"/api/v1/post/{site}/{id}/file": { "get": { "summary": "The image bytes" } },
			"/api/v1/post/{site}/{id}/thumb": { "get": { "summary": "The thumbnail bytes" } },
			"/api/v1/taste": { "get": { "summary": "The taste brief and dial" } },
			"/api/v1/favorite": {
				"post": {
					"summary": "Favorite or unfavorite",
					"requestBody": { "content": { "application/json": { "schema": {
						"type": "object",
						"required": ["site", "id", "on"],
						"properties": { "site": { "type": "string" }, "id": { "type": "integer" }, "on": { "type": "boolean" } }
					} } } }
				}
			},
			"/api/v1/feedback": {
				"post": {
					"summary": "Teach the taste profile",
					"requestBody": { "content": { "application/json": { "schema": {
						"type": "object",
						"required": ["site", "id", "action"],
						"properties": {
							"site": { "type": "string" },
							"id": { "type": "integer" },
							"action": { "type": "string", "enum": ["more", "less", "hide"] }
						}
					} } } }
				}
			},
			"/api/v1/taste/regenerate": { "post": { "summary": "Write a new brief" } }
		}
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	fn raw(method: &str, target: &str, headers: &str, body: &str) -> Vec<u8> {
		format!("{method} {target} HTTP/1.1\r\nHost: x\r\n{headers}Content-Length: {}\r\n\r\n{body}", body.len()).into_bytes()
	}

	#[test]
	fn parses_a_request_and_stops_at_its_body() {
		let bytes = raw("GET", "/api/v1/search?q=fox%20wolf&limit=5", "Authorization: Bearer t\r\n", "");
		let (r, used) = Request::parse(&bytes).unwrap();
		assert_eq!(r.method, Method::Get);
		assert_eq!(r.path, "/api/v1/search");
		assert_eq!(r.param("q"), Some("fox wolf"));
		assert_eq!(r.header("authorization"), Some("Bearer t"));
		assert_eq!(used, bytes.len());
	}

	#[test]
	fn rejects_junk() {
		assert_eq!(Request::parse(b"nope").unwrap_err(), ParseError::Line);
		assert_eq!(Request::parse(b"BREW / HTTP/1.1\r\n\r\n").unwrap_err(), ParseError::Method);
		let big = "A".repeat(MAX_BODY_BYTES + 1);
		assert_eq!(Request::parse(&raw("POST", "/x", "", &big)).unwrap_err(), ParseError::Body);
		assert_eq!(Request::parse(&raw("GET", "api/v1", "", "")).unwrap_err(), ParseError::Line);
	}

	#[test]
	fn routes() {
		assert_eq!(route(Method::Get, "/api/v1/health"), Route::Health);
		assert_eq!(route(Method::Get, "/api/v1/search/"), Route::Search);
		assert_eq!(route(Method::Get, "/api/v1/post/e621/12"), Route::Post { site: "e621".into(), id: 12 });
		assert_eq!(route(Method::Get, "/api/v1/post/e621/12/file"), Route::File { site: "e621".into(), id: 12, thumb: false });
		assert_eq!(route(Method::Get, "/api/v1/post/e621/12/thumb"), Route::File { site: "e621".into(), id: 12, thumb: true });
		assert_eq!(route(Method::Get, "/api/v1/post/e621/12/still"), Route::NotFound);
		assert_eq!(route(Method::Get, "/api/v1/post/e621/x"), Route::NotFound);
		assert_eq!(route(Method::Post, "/api/v1/favorite"), Route::Write(Write::Favorite));
		assert_eq!(route(Method::Get, "/api/v1/favorite"), Route::NotFound);
		assert_eq!(route(Method::Get, "/nope"), Route::NotFound);
	}

	#[test]
	fn tokens_compared_without_short_circuiting() {
		assert!(token_ok("abc", "abc"));
		assert!(!token_ok("abc", "abd"));
		assert!(!token_ok("ab", "abc"));
		assert!(!token_ok("", "abc"));
	}

	#[test]
	fn the_limiter_holds_a_minute() {
		let l = Limiter::new(2);
		let t = Instant::now();
		assert!(l.check(t));
		assert!(l.check(t));
		assert!(!l.check(t));
		assert!(l.check(t + Duration::from_secs(61)));
	}

	struct Fake;
	impl Backend for Fake {
		fn search(&self, req: &SearchReq) -> Result<Vec<Summary>, String> {
			Ok(vec![Summary {
				site: "e926".into(),
				id: 1,
				rating: "s".into(),
				tags: vec![req.text.clone()],
				width: 1,
				height: 1,
				page_url: String::new(),
				file: "/api/v1/post/e926/1/file".into(),
				thumb: "/api/v1/post/e926/1/thumb".into(),
			}])
		}
		fn post(&self, _: &str, _: u64) -> Result<Option<Summary>, String> {
			Ok(None)
		}
		fn file(&self, _: &str, _: u64, _: bool) -> Result<Option<File>, String> {
			Ok(Some(File { content_type: "image/png".into(), bytes: vec![1, 2, 3] }))
		}
		fn taste(&self) -> Result<Option<TasteView>, String> {
			Ok(None)
		}
	}

	fn cx<'a>(l: &'a Limiter, read_only: bool, backend: &'a Fake) -> Context<'a> {
		Context { token: "secret", read_only, limiter: l, backend, now: Instant::now() }
	}

	fn get(target: &str, auth: &str) -> Request {
		Request::parse(&raw("GET", target, &format!("Authorization: {auth}\r\n"), "")).unwrap().0
	}

	#[test]
	fn health_and_schema_need_no_token() {
		let (l, b) = (Limiter::new(10), Fake);
		assert_eq!(handle(&get("/api/v1/health", ""), &cx(&l, true, &b)).status, 200);
		let r = handle(&get("/api/v1/openapi.json", ""), &cx(&l, true, &b));
		assert_eq!(r.status, 200);
		assert!(!String::from_utf8_lossy(&r.body).contains("secret"));
	}

	#[test]
	fn the_rest_needs_the_token() {
		let (l, b) = (Limiter::new(10), Fake);
		assert_eq!(handle(&get("/api/v1/search?q=fox", ""), &cx(&l, true, &b)).status, 401);
		assert_eq!(handle(&get("/api/v1/search?q=fox", "Bearer wrong"), &cx(&l, true, &b)).status, 401);
		assert_eq!(handle(&get("/api/v1/search?q=fox", "Bearer secret"), &cx(&l, true, &b)).status, 200);
	}

	#[test]
	fn search_overrides_are_echoed_and_checked() {
		let (l, b) = (Limiter::new(10), Fake);
		let r = handle(&get("/api/v1/search?q=fox&sites=e926,e621&ratings=s,q", "Bearer secret"), &cx(&l, true, &b));
		assert_eq!(r.status, 200);
		let body: Value = serde_json::from_slice(&r.body).unwrap();
		assert_eq!(body["sites"], json!(["e926", "e621"]));
		assert_eq!(body["ratings"], json!(["s", "q"]));
		let r = handle(&get("/api/v1/search?q=fox&ratings=s,x", "Bearer secret"), &cx(&l, true, &b));
		assert_eq!(r.status, 400);
	}

	#[test]
	fn search_needs_a_query_and_serves_files() {
		let (l, b) = (Limiter::new(10), Fake);
		assert_eq!(handle(&get("/api/v1/search", "Bearer secret"), &cx(&l, true, &b)).status, 400);
		let r = handle(&get("/api/v1/post/e926/1/file", "Bearer secret"), &cx(&l, true, &b));
		assert_eq!(r.status, 200);
		assert_eq!(r.content_type, "image/png");
		assert!(r.to_bytes().ends_with(&[1, 2, 3]));
	}

	#[test]
	fn writes_are_refused_while_read_only() {
		let (l, b) = (Limiter::new(10), Fake);
		let req = Request::parse(&raw("POST", "/api/v1/favorite", "Authorization: Bearer secret\r\n", r#"{"site":"e926","id":1,"on":true}"#))
			.unwrap()
			.0;
		assert_eq!(handle(&req, &cx(&l, true, &b)).status, 409);
	}

	#[test]
	fn a_flood_is_429_with_retry_after() {
		let (l, b) = (Limiter::new(1), Fake);
		assert_eq!(handle(&get("/api/v1/search?q=a", "Bearer secret"), &cx(&l, true, &b)).status, 200);
		let r = handle(&get("/api/v1/search?q=b", "Bearer secret"), &cx(&l, true, &b));
		assert_eq!(r.status, 429);
		assert!(r.headers.iter().any(|(k, _)| *k == "Retry-After"));
	}
}
