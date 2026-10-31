// The engine behind BetterSearch, with no GUI: the search language, the
// planner that turns a search into queries a booru can run, site adapters,
// the local post cache and the tag graph (personal tags and links).

pub mod multi;
mod plan;
pub mod learn;
pub mod similar;
pub mod post;
pub mod query;
pub mod search;
pub mod sites;
pub mod store;
pub mod taggraph;
pub mod taste;
mod wipe;

pub use multi::MultiSearch;
pub use plan::{Plan, plan};
pub use post::{Post, Rating};
pub use query::{Expr, parse};
pub use search::Search;
pub use sites::{Capabilities, Cursor, Site, TagSuggestion};
pub use store::Store;
pub use taggraph::{TagGraph, TagGraphError};

#[derive(Debug, thiserror::Error)]
pub enum Error {
	#[error("{0}")]
	Query(String),
	#[error("network: {0}")]
	Http(reqwest::Error),
	#[error("{site} said: {message}")]
	Api { site: String, message: String },
	#[error("database: {0}")]
	Db(#[from] rusqlite::Error),
	#[error(transparent)]
	Graph(#[from] TagGraphError),
}

/// Network errors lose their URL: Gelbooru's login travels in the URL, and an
/// error shown on screen (or in a screenshot) mustn't carry it.
impl From<reqwest::Error> for Error {
	fn from(e: reqwest::Error) -> Self {
		Error::Http(e.without_url())
	}
}

pub type Result<T, E = Error> = std::result::Result<T, E>;
