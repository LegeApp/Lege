pub mod line;
pub mod outline;
pub mod search;
pub mod selection;

pub use line::{
    CharacterGeometry, LineBox, LineSource, PageLineSet, TextSubstrate, cluster_lines,
    transform_characters_to_document,
};
pub use outline::OutlineSynthesizer;
pub use search::{SearchHit, SearchIndex, SearchService};
pub use selection::{PageSelection, SelectionModel, TextPosition, hit_test};
