// Windows Store In-App Purchase (Consumable Donation) integration
// Purchase a Store-managed consumable add-on and immediately fulfill one unit.
//
// Uses windows-rs tag 71 with windows_future for async operations.
// Call these from the UI thread (e.g., your Dioxus button handler).

#[cfg(target_os = "windows")]
mod windows_store_iap {
    use uuid::Uuid;
    use windows::{
        Services::Store::*,
        Win32::Foundation::HWND,
        Win32::UI::Shell::IInitializeWithWindow,
        core::{GUID, HSTRING, Interface},
    };

    /// Optionally associate a StoreContext with your HWND (best effort).
    fn try_initialize_with_window(ctx: &StoreContext, hwnd: HWND) {
        if !hwnd.0.is_null() {
            if let Ok(init) = ctx.cast::<IInitializeWithWindow>() {
                unsafe {
                    let _ = init.Initialize(hwnd);
                }
            }
        }
    }

    /// Get a StoreContext and anchor it to your app window (if possible).
    fn get_ctx(hwnd: HWND) -> windows::core::Result<StoreContext> {
        let ctx = StoreContext::GetDefault()?;
        try_initialize_with_window(&ctx, hwnd);
        Ok(ctx)
    }

    /// Purchase by Store ID (recommended since you have it, e.g. "9NQ3GXX3LMDJ")
    pub async fn purchase_and_consume_by_store_id(
        hwnd: HWND,
        store_id: &str,
    ) -> windows::core::Result<()> {
        let ctx = get_ctx(hwnd)?;
        let purchase = ctx.RequestPurchaseAsync(&HSTRING::from(store_id))?.await?;

        match purchase.Status()? {
            StorePurchaseStatus::Succeeded | StorePurchaseStatus::AlreadyPurchased => {
                // Immediately consume 1 unit so the user can donate again
                let tracking_guid = GUID::from_u128(Uuid::new_v4().as_u128());
                let _fulfill = ctx
                    .ReportConsumableFulfillmentAsync(&HSTRING::from(store_id), 1, tracking_guid)?
                    .await?;
                Ok(())
            }
            StorePurchaseStatus::NotPurchased => {
                // User canceled
                Err(windows::core::Error::from_hresult(
                    windows::Win32::Foundation::E_ABORT,
                ))
            }
            StorePurchaseStatus::NetworkError => Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            )),
            StorePurchaseStatus::ServerError => Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            )),
            _ => Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            )),
        }
    }

    /// Purchase by your Partner Center "Product ID" (In-app offer token), e.g. "donation_500".
    /// Note: This implementation is simplified - querying by token requires iterating products.
    /// For now, we just return an error to fall back to the Store ID method.
    pub async fn purchase_and_consume_by_token(
        _hwnd: HWND,
        _token: &str,
    ) -> windows::core::Result<()> {
        // TODO: Implement product querying once collection APIs are working
        // For now, use purchase_by_store_id instead
        Err(windows::core::Error::from_hresult(
            windows::Win32::Foundation::E_NOTIMPL,
        ))
    }
}

#[cfg(target_os = "windows")]
pub use windows_store_iap::*;

#[cfg(not(target_os = "windows"))]
pub async fn purchase_and_consume_by_store_id(_: (), _: &str) -> Result<(), ()> {
    Err(())
}

#[cfg(not(target_os = "windows"))]
pub async fn purchase_and_consume_by_token(_: (), _: &str) -> Result<(), ()> {
    Err(())
}
