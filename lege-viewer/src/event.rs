#[derive(Debug, Clone)]
pub enum ViewerEvent {
    Wake,
    FatalBackgroundError(String),
}
