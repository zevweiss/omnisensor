// Relatively generic convenience types

use std::collections::HashSet;

pub type ErrResult<T> = Result<T, Box<dyn std::error::Error>>;

// Could just use Option<T> for this with None meaning all, but this is a bit
// more explicit ("None == All" is sorta counterintuitive, after all...), and
// allows more natural, readable querying via FilterSet::contains().
#[derive(Debug)]
pub enum FilterSet<T> {
	All,
	Only(HashSet<T>),
}

impl<T: Eq + std::hash::Hash> FilterSet<T> {
	pub fn contains(&self, x: &T) -> bool {
		match self {
			Self::All => true,
			Self::Only(set) => set.contains(x),
		}
	}
}

// If the Option is Some but the set is empty, you get what you asked for (a
// filter that rejects everything).
impl<T> From<Option<HashSet<T>>> for FilterSet<T> {
	fn from(set: Option<HashSet<T>>) -> Self {
		match set {
			Some(set) => Self::Only(set),
			None => Self::All,
		}
	}
}
