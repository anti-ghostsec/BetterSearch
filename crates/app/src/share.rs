// The FerroOS share box (FerroOS 1.0.57 and later). Other systems don't have
// one, so there the Share button isn't shown at all.
//
// The tool is always called by its full path and never waited on: it exits 0
// whatever happens and says nothing back, by design. A file in memory goes in
// on its stdin, never through a temporary file.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use bettersearch_core::Post;

pub const TOOL: &str = "/usr/local/bin/ferro-share";

/// The share box, if this system has one.
pub fn tool() -> Option<PathBuf> {
	let path = Path::new(TOOL);
	let runnable = path.metadata().is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0);
	runnable.then(|| path.to_path_buf())
}

/// Offers bytes as a file called `name`; its extension tells the receiving
/// app the type.
pub fn bytes(tool: &Path, name: &str, bytes: Vec<u8>) -> std::io::Result<()> {
	let mut child = Command::new(tool).arg("--stdin").arg(name).stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
	let mut stdin = child.stdin.take().expect("stdin is piped");
	// The box may read slowly (it waits for a pick), so the bytes go in off
	// this thread, which then reaps it.
	std::thread::spawn(move || {
		let _ = stdin.write_all(&bytes);
		drop(stdin);
		let _ = child.wait();
	});
	Ok(())
}

/// Offers files already on disk, all in one box.
pub fn files(tool: &Path, paths: &[PathBuf]) -> std::io::Result<()> {
	let mut child = Command::new(tool).arg("--").args(paths).stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null()).spawn()?;
	std::thread::spawn(move || {
		let _ = child.wait();
	});
	Ok(())
}

/// `site_id.ext` for a post's file. Both parts come from the site, so anything
/// but letters, digits, `-` and `_` becomes `_`: no answer can name a path.
/// `ext` is the file's real type when the bytes show it.
pub fn file_name(post: &Post, bytes: Option<&[u8]>) -> String {
	let clean = |s: &str| -> String { s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' }).collect() };
	let real = bytes.and_then(|b| image::guess_format(b).ok()).and_then(|f| f.extensions_str().first().copied());
	let ext = clean(real.unwrap_or(&post.ext));
	let stem = format!("{}_{}", clean(&post.site), post.id);
	if ext.is_empty() { stem } else { format!("{stem}.{ext}") }
}

/// A stand-in for the share box that writes down its stdin and arguments, as
/// `stdin` and `args` in `dir`. `args` comes last, so once it's there both are complete.
#[cfg(test)]
pub fn fake_tool(dir: &Path) -> PathBuf {
	let tool = dir.join("ferro-share");
	let script = format!(
		"#!/bin/sh\ncat > '{0}/stdin.tmp'\nmv '{0}/stdin.tmp' '{0}/stdin'\nprintf '%s\\n' \"$@\" > '{0}/args.tmp'\nmv '{0}/args.tmp' '{0}/args'\n",
		dir.display()
	);
	std::fs::write(&tool, script).unwrap();
	std::fs::set_permissions(&tool, std::fs::Permissions::from_mode(0o755)).unwrap();
	tool
}

/// A file's text once it's there (bytes that aren't UTF-8 as �), waiting up
/// to four seconds.
#[cfg(test)]
pub fn wait_for(path: &Path) -> String {
	for _ in 0..200 {
		if let Ok(b) = std::fs::read(path) {
			return String::from_utf8_lossy(&b).into_owned();
		}
		std::thread::sleep(std::time::Duration::from_millis(20));
	}
	panic!("{} never appeared", path.display());
}

#[cfg(test)]
mod tests {
	use super::*;

	fn post(site: &str, ext: &str) -> Post {
		Post {
			site: site.into(),
			id: 7,
			tags: Vec::new(),
			categories: Default::default(),
			rating: bettersearch_core::Rating::Safe,
			score: 0,
			fav_count: 0,
			width: 1,
			height: 1,
			ext: ext.into(),
			md5: None,
			file_url: None,
			sample_url: None,
			preview_url: None,
			page_url: String::new(),
			sources: Vec::new(),
			created_at: String::new(),
		}
	}

	#[test]
	fn names_stay_one_plain_file() {
		assert_eq!(file_name(&post("e926", "png"), None), "e926_7.png");
		assert_eq!(file_name(&post("../x", "/../../.bashrc"), None), "___x_7.________bashrc");
		assert_eq!(file_name(&post("e926", ""), None), "e926_7");
		// What the bytes are beats what the site said.
		assert_eq!(file_name(&post("realbooru", "jpeg"), Some(b"\x89PNG\r\n\x1a\n....")), "realbooru_7.png");
		assert_eq!(file_name(&post("e926", "webm"), Some(b"\x1a\x45\xdf\xa3")), "e926_7.webm");
	}

	#[test]
	fn hands_bytes_on_stdin_and_files_as_arguments() {
		let dir = std::env::temp_dir().join(format!("bs-share-{}", std::process::id()));
		std::fs::create_dir_all(&dir).unwrap();
		let tool = fake_tool(&dir);
		bytes(&tool, "e926_7.png", b"picture".to_vec()).unwrap();
		assert_eq!(wait_for(&dir.join("args")), "--stdin\ne926_7.png\n");
		assert_eq!(wait_for(&dir.join("stdin")), "picture");
		std::fs::remove_file(dir.join("args")).unwrap();
		std::fs::remove_file(dir.join("stdin")).unwrap();
		files(&tool, &[dir.join("a.png"), dir.join("-b.png")]).unwrap();
		assert_eq!(wait_for(&dir.join("args")), format!("--\n{0}/a.png\n{0}/-b.png\n", dir.display()));
		std::fs::remove_dir_all(&dir).unwrap();
	}
}
