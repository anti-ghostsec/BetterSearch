// SQLite's memory, wiped as it's handed back. The decrypted database lives in
// SQLite's heap (the image itself, its page cache, the copies made to save
// it), and SQLite's own allocator frees those blocks with the data still in
// them, for anything later to read. Here every block carries its size and is
// zeroed before it's freed or moved, so closing a database (locking) clears it.
//
// It has to go in before SQLite starts, so every connection is opened here.

use std::alloc::{Layout, alloc, dealloc};
use std::ffi::{c_int, c_void};
use std::path::Path;
use std::sync::OnceLock;

use rusqlite::{Connection, ffi};
use zeroize::Zeroize;

/// Room before each block for its size. Keeps the 8-byte alignment SQLite needs.
const HEAD: usize = 16;

pub fn open(path: &Path) -> rusqlite::Result<Connection> {
	install();
	Connection::open(path)
}

pub fn open_in_memory() -> rusqlite::Result<Connection> {
	install();
	Connection::open_in_memory()
}

/// Puts the wiping allocator in, once. False if SQLite had already started
/// (something opened a database without coming through here) and kept its own.
pub fn install() -> bool {
	static INSTALLED: OnceLock<bool> = OnceLock::new();
	*INSTALLED.get_or_init(|| {
		let methods = ffi::sqlite3_mem_methods {
			xMalloc: Some(block_alloc),
			xFree: Some(block_free),
			xRealloc: Some(block_realloc),
			xSize: Some(block_size),
			xRoundup: Some(round_up),
			xInit: Some(init),
			xShutdown: Some(shutdown),
			pAppData: std::ptr::null_mut(),
		};
		// SQLite copies the methods, so they needn't outlive the call.
		let ok = unsafe { ffi::sqlite3_config(ffi::SQLITE_CONFIG_MALLOC, &methods as *const ffi::sqlite3_mem_methods) } == ffi::SQLITE_OK;
		debug_assert!(ok, "SQLite started before its memory could be made to wipe");
		ok
	})
}

fn layout(size: usize) -> Layout {
	Layout::from_size_align(size + HEAD, HEAD).expect("SQLite asks for under 2 GiB at a time")
}

/// The size a block was made with.
unsafe fn size_of(p: *mut c_void) -> usize {
	unsafe { p.cast::<u8>().sub(HEAD).cast::<usize>().read() }
}

unsafe extern "C" fn block_alloc(n: c_int) -> *mut c_void {
	let size = n.max(0) as usize;
	let p = unsafe { alloc(layout(size)) };
	if p.is_null() {
		return std::ptr::null_mut();
	}
	unsafe {
		p.cast::<usize>().write(size);
		p.add(HEAD).cast()
	}
}

unsafe extern "C" fn block_free(p: *mut c_void) {
	if p.is_null() {
		return;
	}
	unsafe {
		let size = size_of(p);
		// Volatile writes, so the compiler can't drop them as dead before the free.
		std::slice::from_raw_parts_mut(p.cast::<u8>(), size).zeroize();
		#[cfg(test)]
		let _ = WIPED.try_with(|w| w.set(w.get() + size));
		dealloc(p.cast::<u8>().sub(HEAD), layout(size));
	}
}

/// Always a new block: the system realloc can move one and free the old
/// copy without wiping it.
unsafe extern "C" fn block_realloc(p: *mut c_void, n: c_int) -> *mut c_void {
	let new = unsafe { block_alloc(n) };
	if !new.is_null() {
		unsafe {
			std::ptr::copy_nonoverlapping(p.cast::<u8>(), new.cast::<u8>(), size_of(p).min(n.max(0) as usize));
			block_free(p);
		}
	}
	new
}

unsafe extern "C" fn block_size(p: *mut c_void) -> c_int {
	if p.is_null() { 0 } else { unsafe { size_of(p) as c_int } }
}

unsafe extern "C" fn round_up(n: c_int) -> c_int {
	n.saturating_add(7) & !7
}

unsafe extern "C" fn init(_: *mut c_void) -> c_int {
	ffi::SQLITE_OK
}

unsafe extern "C" fn shutdown(_: *mut c_void) {}

#[cfg(test)]
thread_local! {
	/// Bytes this thread has wiped, to see SQLite's frees come through here.
	static WIPED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::Store;

	#[test]
	fn closing_a_database_wipes_it() {
		let mut s = Store::in_memory().unwrap();
		assert!(install(), "SQLite kept its own allocator");
		s.set_value("big", &"secret ".repeat(100_000)).unwrap();
		let bytes = s.to_bytes().unwrap();
		drop(s);
		let back = Store::from_bytes(&bytes).unwrap();
		let before = WIPED.get();
		drop(back);
		// At least the database image itself, besides its page cache.
		assert!(WIPED.get() - before >= bytes.len(), "wiped {} of {}", WIPED.get() - before, bytes.len());
	}

	#[test]
	fn moved_blocks_keep_their_bytes() {
		install();
		unsafe {
			let p = block_alloc(round_up(10)).cast::<u8>();
			std::ptr::copy_nonoverlapping(b"0123456789".as_ptr(), p, 10);
			let q = block_realloc(p.cast(), round_up(5000)).cast::<u8>();
			assert_eq!(std::slice::from_raw_parts(q, 10), b"0123456789");
			assert_eq!(block_size(q.cast()), 5000);
			block_free(q.cast());
		}
	}
}
