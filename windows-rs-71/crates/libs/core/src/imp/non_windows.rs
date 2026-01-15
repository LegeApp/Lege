use crate::{IUnknown, IUnknown_Vtbl, Interface, GUID, HRESULT};
use core::ffi::c_void;
use core::ptr::null_mut;

/// Stubbed Free Threaded Marshaler helper for non-Windows targets.
#[allow(unused_variables)]
pub unsafe fn marshaler(_outer: IUnknown, result: *mut *mut c_void) -> HRESULT {
    if !result.is_null() {
        *result = null_mut();
    }
    HRESULT::from_win32(0)
}

#[repr(transparent)]
#[derive(Clone)]
pub struct IMarshal(pub IUnknown);

unsafe impl Interface for IMarshal {
    type Vtable = IMarshal_Vtbl;
    const IID: GUID = GUID::from_u128(0);
}

#[repr(C)]
pub struct IMarshal_Vtbl {
    pub base__: IUnknown_Vtbl,
    pub GetUnmarshalClass: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        *const c_void,
        u32,
        *const c_void,
        u32,
        *mut GUID,
    ) -> HRESULT,
    pub GetMarshalSizeMax: unsafe extern "system" fn(
        *mut c_void,
        *const GUID,
        *const c_void,
        u32,
        *const c_void,
        u32,
        *mut u32,
    ) -> HRESULT,
    pub MarshalInterface: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        *const GUID,
        *const c_void,
        u32,
        *const c_void,
        u32,
    ) -> HRESULT,
    pub UnmarshalInterface: unsafe extern "system" fn(
        *mut c_void,
        *mut c_void,
        *const GUID,
        *mut *mut c_void,
    ) -> HRESULT,
    pub ReleaseMarshalData: unsafe extern "system" fn(*mut c_void, *mut c_void) -> HRESULT,
    pub DisconnectObject: unsafe extern "system" fn(*mut c_void, u32) -> HRESULT,
}
