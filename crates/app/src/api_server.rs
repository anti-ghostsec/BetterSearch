//! The socket side of the local API: accept, parse, hand to `api::handle`.
//!
//! Loopback by default, one thread per connection behind a small cap, read and
//! write timeouts so a stalled caller can't pin a thread. The token is made
//! here, and a `api.json` (mode 0600) beside the vault tells another program
//! the port and the token — it holds nothing else.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bettersearch_core::api::{self, Backend, Context, File, Limiter, Request, Summary, TasteView, MAX_BODY_BYTES, MAX_HEADER_BYTES};
use bettersearch_core::{MultiSearch, Post, Rating, Site, Store, taste};

use crate::cache::Cache;
use crate::settings::{self, Settings};

/// At most this many calls are served at once.
const CONNECTIONS: usize = 4;
const READ_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Logged {
	pub status: u16,
	pub method: &'static str,
	pub path: String,
	pub ms: u128,
}

/// What the API does for a request: searches, reads posts, fetches bytes.
pub struct Live {
	store: Arc<Mutex<Store>>,
	cache: Mutex<Cache>,
}

impl Live {
	fn settings(&self) -> Settings {
		Settings::load(&self.store.lock().unwrap_or_else(|e| e.into_inner()))
	}

	fn sites(&self, wanted: &[String], allowed: &[String]) -> Vec<Arc<dyn Site>> {
		let picked: &[String] = if wanted.is_empty() { allowed } else { wanted };
		settings::make_sites(&self.settings())
			.into_iter()
			.filter(|s| picked.iter().any(|id| id == s.id()))
			.collect()
	}

	fn summary(&self, p: &Post) -> Summary {
		Summary {
			site: p.site.clone(),
			id: p.id,
			rating: p.rating.letter().to_string(),
			tags: p.tags.clone(),
			width: p.width,
			height: p.height,
			page_url: p.page_url.clone(),
			file: format!("/api/v1/post/{}/{}/file", p.site, p.id),
			thumb: format!("/api/v1/post/{}/{}/thumb", p.site, p.id),
		}
	}
}

impl Backend for Live {
	fn search(&self, req: &api::SearchReq) -> Result<Vec<Summary>, String> {
		let settings = self.settings();
		let sites = self.sites(&req.sites, &settings.sites);
		if sites.is_empty() {
			return Err("no site matched".into());
		}
		let ratings: Vec<Rating> = if req.ratings.is_empty() { settings.ratings.clone() } else { req.ratings.clone() };
		if ratings.is_empty() {
			return Err("tick at least one rating".into());
		}
		let mut search = MultiSearch::start(&sites, &req.text, &ratings, &self.store).map_err(|e| e.to_string())?;
		let posts = search.next(req.limit, &self.store).map_err(|e| e.to_string())?;
		Ok(posts.iter().map(|p| self.summary(p)).collect())
	}

	fn post(&self, site: &str, id: u64) -> Result<Option<Summary>, String> {
		let post = self.store.lock().unwrap_or_else(|e| e.into_inner()).post(&format!("{site}:post/{id}")).map_err(|e| e.to_string())?;
		Ok(post.as_ref().map(|p| self.summary(p)))
	}

	fn file(&self, site: &str, id: u64, thumb: bool) -> Result<Option<File>, String> {
		let key = format!("{site}:post/{id}");
		let cache_key = format!("{key}:{}", if thumb { "t" } else { "f" });
		if let Some(bytes) = self.cache.lock().unwrap_or_else(|e| e.into_inner()).get(&cache_key) {
			return Ok(Some(File { content_type: sniff(&bytes).into(), bytes: bytes.to_vec() }));
		}
		let Some(post) = self.store.lock().unwrap_or_else(|e| e.into_inner()).post(&key).map_err(|e| e.to_string())? else {
			return Ok(None);
		};
		let url = if thumb {
			post.preview_url.or(post.sample_url).or(post.file_url)
		} else {
			post.file_url.or(post.sample_url)
		};
		let Some(url) = url else { return Ok(Some(File { content_type: "text/plain".into(), bytes: Vec::new() })) };
		let site = settings::make_sites(&self.settings()).into_iter().find(|s| s.id() == site);
		let Some(site) = site else { return Err("unknown site".into()) };
		let bytes = site.fetch_bytes(&url).map_err(|e| e.to_string())?;
		let kind = sniff(&bytes).to_string();
		self.cache.lock().unwrap_or_else(|e| e.into_inner()).put(&cache_key, bytes.clone());
		Ok(Some(File { content_type: kind, bytes }))
	}

	fn taste(&self) -> Result<Option<TasteView>, String> {
		let settings = self.settings();
		if !settings.taste.enabled {
			return Ok(None);
		}
		let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
		let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
		let t = taste::build(&store, &settings.ratings, now, settings.taste.dial_override).map_err(|e| e.to_string())?;
		let brief = store.value("taste_brief").ok().flatten().unwrap_or_default();
		Ok(Some(TasteView {
			brief,
			dial: t.dial,
			intensity: t.intensity,
			tags: t.tags.iter().take(24).map(|t| t.tag.clone()).collect(),
			updated_at: now,
		}))
	}

	fn write(&self, write: api::Write, body: serde_json::Value) -> Result<serde_json::Value, String> {
		match write {
			api::Write::Favorite => {
				let site = body["site"].as_str().ok_or("site is required")?;
				let id = body["id"].as_u64().ok_or("id is required")?;
				let on = body["on"].as_bool().ok_or("on is required")?;
				let store = self.store.lock().unwrap_or_else(|e| e.into_inner());
				let post = store.post(&format!("{site}:post/{id}")).map_err(|e| e.to_string())?.ok_or("no such post")?;
				store.set_favorite(&post, on).map_err(|e| e.to_string())?;
				Ok(serde_json::json!({ "favorite": on }))
			}
			api::Write::Feedback => Ok(serde_json::json!({ "recorded": false })),
			api::Write::Regenerate => Err("no model configured yet".into()),
		}
	}
}

/// The type from the first bytes, so callers aren't handed HTML as a picture.
fn sniff(bytes: &[u8]) -> &'static str {
	match bytes {
		[0x89, b'P', b'N', b'G', ..] => "image/png",
		[0xff, 0xd8, ..] => "image/jpeg",
		[b'G', b'I', b'F', ..] => "image/gif",
		[b'R', b'I', b'F', b'F', ..] => "image/webp",
		[0x1a, 0x45, 0xdf, 0xa3, ..] => "video/webm",
		_ => "application/octet-stream",
	}
}

pub struct Server {
	addr: std::net::SocketAddr,
	stop: Arc<AtomicBool>,
	live: Arc<AtomicUsize>,
	log: Arc<Mutex<VecDeque<Logged>>>,
	join: Option<std::thread::JoinHandle<()>>,
	discovery: Option<std::path::PathBuf>,
}

impl Server {
	/// Binds and starts serving. `enabled` addresses are the caller's problem.
	pub fn start(api_settings: &settings::Api, store: Arc<Mutex<Store>>) -> std::io::Result<Server> {
		Self::start_in(api_settings, store, settings::data_dir())
	}

	/// `start`, with the discovery file put somewhere else (tests).
	pub fn start_in(api_settings: &settings::Api, store: Arc<Mutex<Store>>, dir: std::path::PathBuf) -> std::io::Result<Server> {
		let host = if api_settings.lan { "0.0.0.0" } else { "127.0.0.1" };
		let listener = TcpListener::bind((host, api_settings.port))?;
		listener.set_nonblocking(false)?;
		let addr = listener.local_addr()?;

		let backend = Arc::new(Live { store, cache: Mutex::new(Cache::new(crate::cache::CAP)) });
		let stop = Arc::new(AtomicBool::new(false));
		let live = Arc::new(AtomicUsize::new(0));
		let log = Arc::new(Mutex::new(VecDeque::new()));
		let limiter = Arc::new(Limiter::new(api_settings.per_minute));
		let token = if api_settings.token.trim().is_empty() { new_token() } else { api_settings.token.clone() };

		let discovery = write_discovery(&dir, addr.port(), &token).ok();

		let worker = {
			let (stop, live, log, backend) = (stop.clone(), live.clone(), log.clone(), backend.clone());
			let (read_only, token) = (api_settings.read_only, token.clone());
			std::thread::spawn(move || {
				for stream in listener.incoming() {
					if stop.load(Ordering::Relaxed) {
						break;
					}
					let Ok(stream) = stream else { continue };
					if live.load(Ordering::Relaxed) >= CONNECTIONS {
						let _ = stream.shutdown(Shutdown::Both);
						continue;
					}
					let (live, log, backend, limiter, token) = (live.clone(), log.clone(), backend.clone(), limiter.clone(), token.clone());
					live.fetch_add(1, Ordering::Relaxed);
					std::thread::spawn(move || {
						serve(stream, &backend, &token, read_only, &limiter, &log);
						live.fetch_sub(1, Ordering::Relaxed);
					});
				}
			})
		};

		Ok(Server { addr, stop, live, log, join: Some(worker), discovery })
	}

	pub fn port(&self) -> u16 {
		self.addr.port()
	}

	pub fn busy(&self) -> usize {
		self.live.load(Ordering::Relaxed)
	}

	/// The newest requests, newest first. Path but not query: the query is the taste.
	pub fn log(&self) -> Vec<(u16, String, u128)> {
		let log = self.log.lock().unwrap_or_else(|e| e.into_inner());
		log.iter().rev().map(|l| (l.status, format!("{} {}", l.method, l.path), l.ms)).collect()
	}
}

impl Drop for Server {
	fn drop(&mut self) {
		self.stop.store(true, Ordering::Relaxed);
		// Wake the blocking accept.
		let _ = TcpStream::connect(self.addr);
		if let Some(join) = self.join.take() {
			let _ = join.join();
		}
		if let Some(path) = &self.discovery {
			let _ = std::fs::remove_file(path);
		}
	}
}

fn serve(mut stream: TcpStream, backend: &Live, token: &str, read_only: bool, limiter: &Limiter, log: &Mutex<VecDeque<Logged>>) {
	let _ = stream.set_read_timeout(Some(READ_TIMEOUT));
	let _ = stream.set_write_timeout(Some(READ_TIMEOUT));
	let started = Instant::now();

	let mut raw = Vec::new();
	let mut chunk = [0u8; 8192];
	loop {
		match stream.read(&mut chunk) {
			Ok(0) => break,
			Ok(n) => {
				raw.extend_from_slice(&chunk[..n]);
				if raw.len() > MAX_HEADER_BYTES + MAX_BODY_BYTES + 1024 {
					break;
				}
				// Stop once the head says the body is here.
				if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
					let need = std::str::from_utf8(&raw[..end])
						.ok()
						.and_then(|head| {
							head.lines().find_map(|l| {
								let (k, v) = l.split_once(':')?;
								k.eq_ignore_ascii_case("content-length").then(|| v.trim().parse::<usize>().ok())?
							})
						})
						.unwrap_or(0);
					if raw.len() >= end + 4 + need {
						break;
					}
				}
			}
			Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => break,
			Err(_) => break,
		}
	}

	let response = match Request::parse(&raw) {
		Ok((req, _)) => {
			let cx = Context { token, read_only, limiter, backend, now: Instant::now() };
			let res = api::handle(&req, &cx);
			let mut log = log.lock().unwrap_or_else(|e| e.into_inner());
			log.push_back(Logged { status: res.status, method: if req.method == api::Method::Post { "POST" } else { "GET" }, path: req.path, ms: started.elapsed().as_millis() });
			while log.len() > 100 {
				log.pop_front();
			}
			res
		}
		Err(e) => api::Response::error(400, &e.to_string()),
	};
	let _ = stream.write_all(&response.to_bytes());
	let _ = stream.flush();
}

pub fn new_token() -> String {
	let mut bytes = [0u8; 32];
	if let Ok(mut f) = std::fs::File::open("/dev/urandom") {
		let _ = f.read_exact(&mut bytes);
	}
	bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Tells another program where to call and with what. Mode 0600, nothing but
/// the port and token, removed when the server stops.
fn write_discovery(dir: &std::path::Path, port: u16, token: &str) -> std::io::Result<std::path::PathBuf> {
	std::fs::create_dir_all(dir)?;
	let path = dir.join("api.json");
	let mut opts = std::fs::OpenOptions::new();
	opts.write(true).create(true).truncate(true);
	use std::os::unix::fs::OpenOptionsExt as _;
	opts.mode(0o600);
	let mut file = opts.open(&path)?;
	writeln!(file, "{}", serde_json::json!({ "port": port, "token": token, "api": "/api/v1", "pid": std::process::id() }))?;
	Ok(path)
}

#[cfg(test)]
mod tests {
	use super::*;

	fn call(port: u16, path: &str, auth: Option<&str>) -> (u16, String) {
		let mut s = TcpStream::connect(("127.0.0.1", port)).unwrap();
		let auth = auth.map(|t| format!("Authorization: Bearer {t}\r\n")).unwrap_or_default();
		write!(s, "GET {path} HTTP/1.1\r\nHost: x\r\n{auth}Content-Length: 0\r\n\r\n").unwrap();
		let mut raw = String::new();
		s.read_to_string(&mut raw).unwrap();
		let status = raw.split(' ').nth(1).and_then(|c| c.parse().ok()).unwrap_or(0);
		(status, raw)
	}

	/// Each test gets its own directory, so one dropping can't delete the
	/// other's `api.json`.
	fn up() -> (Server, Arc<Mutex<Store>>, std::path::PathBuf) {
		use std::sync::atomic::{AtomicUsize, Ordering};
		static N: AtomicUsize = AtomicUsize::new(0);
		let dir = std::env::temp_dir().join(format!("bs-api-{}-{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
		let store = Arc::new(Mutex::new(Store::in_memory().unwrap()));
		let cfg = settings::Api { enabled: true, token: "test-token".into(), ..Default::default() };
		let server = Server::start_in(&cfg, store.clone(), dir.clone()).unwrap();
		(server, store, dir)
	}

	#[test]
	fn serves_health_open_and_everything_else_closed() {
		let (server, _store, _dir) = up();
		let port = server.port();
		assert_eq!(call(port, "/api/v1/health", None).0, 200);
		assert_eq!(call(port, "/api/v1/openapi.json", None).0, 200);
		assert_eq!(call(port, "/api/v1/search?q=fox", None).0, 401);
		assert_eq!(call(port, "/api/v1/search?q=fox", Some("wrong")).0, 401);
	}

	#[test]
	fn announces_itself_and_cleans_up() {
		let (server, _store, dir) = up();
		let path = dir.join("api.json");
		let text = std::fs::read_to_string(&path).unwrap();
		assert!(text.contains("\"port\"") && text.contains("test-token"));
		let port = server.port();
		drop(server);
		assert!(!path.exists(), "api.json outlives the server");
		assert!(TcpStream::connect(("127.0.0.1", port)).is_err());
		let _ = std::fs::remove_dir_all(&dir);
	}
}
