pub mod line;
pub mod search;
pub mod selection;

pub use line::{
    CharacterGeometry, LineBox, LineSource, PageLineSet, TextSubstrate, cluster_lines, transform_characters_to_document,
};
pub use search::{SearchHit, SearchIndex};
pub use selection::{PageSelection, SelectionModel, TextPosition};
