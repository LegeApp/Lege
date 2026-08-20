//
// File Save
//

use crate::{
    FileHandle,
    backend::{AsyncFileSaveDialogImpl, DialogFutureType},
    file_dialog::FileDialog,
};
use std::future::ready;
impl AsyncFileSaveDialogImpl for FileDialog {
    fn save_file_async(self) -> DialogFutureType<Option<FileHandle>> {
        let file = FileHandle::writable(self);
        Box::pin(ready(Some(file)))
    }
}
