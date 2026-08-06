#[derive(Debug, Clone)]
pub enum ViewerEvent {
    Wake,
    FatalBackgroundError(String),
    Processing(crate::processing::ProcessingUpdate),
}
