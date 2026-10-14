//! Dragging a result out of Nuiter (into Explorer, a mail, a chat): the shell's own data object for the file, and a drop
//! source that follows the mouse.

use windows::Win32::Foundation::{DRAGDROP_S_CANCEL, DRAGDROP_S_DROP, DRAGDROP_S_USEDEFAULTCURSORS};
use windows::Win32::System::Com::IDataObject;
use windows::Win32::System::Ole::{DROPEFFECT, DROPEFFECT_COPY, DROPEFFECT_LINK, DoDragDrop, IDropSource, IDropSource_Impl};
use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
use windows::Win32::UI::Shell::{BHID_DataObject, IShellItem, SHCreateItemFromParsingName};
use windows::core::{BOOL, HRESULT, HSTRING, implement};

#[implement(IDropSource)]
struct Source;

impl IDropSource_Impl for Source_Impl {
    fn QueryContinueDrag(&self, escape: BOOL, keys: MODIFIERKEYS_FLAGS) -> HRESULT {
        if escape.as_bool() {
            return DRAGDROP_S_CANCEL;
        }
        if keys.0 & MK_LBUTTON.0 == 0 {
            return DRAGDROP_S_DROP;
        }
        HRESULT(0)
    }

    fn GiveFeedback(&self, _effect: DROPEFFECT) -> HRESULT {
        DRAGDROP_S_USEDEFAULTCURSORS
    }
}

/// Drags the file until it's dropped or cancelled (blocks the UI thread meanwhile, as drags do).
pub fn drag_file(path: &str) {
    // SAFETY: OLE drag and drop on the (STA) UI thread with the shell's data object.
    unsafe {
        let Ok(item) = SHCreateItemFromParsingName::<_, _, IShellItem>(&HSTRING::from(path), None) else { return };
        let Ok(data) = item.BindToHandler::<_, IDataObject>(None, &BHID_DataObject) else { return };
        let source: IDropSource = Source.into();
        let mut effect = DROPEFFECT(0);
        let _ = DoDragDrop(&data, &source, DROPEFFECT_COPY | DROPEFFECT_LINK, &mut effect);
    }
}
