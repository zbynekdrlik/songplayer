//! Tests for the error classification (#223 S1a).

use super::{
    DXGI_ERROR_DEVICE_HUNG, DXGI_ERROR_DEVICE_REMOVED, DXGI_ERROR_DEVICE_RESET,
    DXGI_ERROR_DRIVER_INTERNAL_ERROR, GpuError, is_device_lost,
};

/// `E_INVALIDARG`: an ordinary failure, the device lives on.
const E_INVALIDARG: u32 = 0x8007_0057;
/// `DXGI_ERROR_NOT_FOUND`: also not a lost device.
const DXGI_ERROR_NOT_FOUND: u32 = 0x887A_0002;

#[test]
fn the_device_removed_family_is_a_lost_device() {
    for hresult in [
        DXGI_ERROR_DEVICE_REMOVED,
        DXGI_ERROR_DEVICE_HUNG,
        DXGI_ERROR_DEVICE_RESET,
        DXGI_ERROR_DRIVER_INTERNAL_ERROR,
    ] {
        assert!(is_device_lost(hresult), "{hresult:#x}");
        let error = GpuError::from_hresult("Draw", hresult);
        assert_eq!(
            error,
            GpuError::DeviceLost {
                call: "Draw",
                hresult
            }
        );
        assert!(error.is_device_lost());
    }
}

#[test]
fn any_other_failure_is_an_api_error() {
    for hresult in [E_INVALIDARG, DXGI_ERROR_NOT_FOUND, 0] {
        assert!(!is_device_lost(hresult), "{hresult:#x}");
        let error = GpuError::from_hresult("CreateTexture2D", hresult);
        assert_eq!(
            error,
            GpuError::Api {
                call: "CreateTexture2D",
                hresult
            }
        );
        assert!(!error.is_device_lost());
    }
    assert!(!GpuError::Unsupported.is_device_lost());
}

#[test]
fn any_reason_the_device_reports_for_its_removal_is_a_lost_device() {
    // DXGI_ERROR_INVALID_CALL: the app's own bad call removed the device.
    const DXGI_ERROR_INVALID_CALL: u32 = 0x887A_0001;
    for hresult in [
        DXGI_ERROR_INVALID_CALL,
        DXGI_ERROR_DEVICE_REMOVED,
        E_INVALIDARG,
    ] {
        let error = GpuError::removed(hresult);
        assert_eq!(
            error,
            GpuError::DeviceLost {
                call: "GetDeviceRemovedReason",
                hresult
            }
        );
        assert!(error.is_device_lost());
    }
}

#[test]
fn the_dxgi_codes_are_the_sdk_values() {
    // dxgi.h / winerror.h: _FACILDXGI (0x87A), codes 5, 6, 7 and 0x20.
    assert_eq!(DXGI_ERROR_DEVICE_REMOVED, 0x887A_0005);
    assert_eq!(DXGI_ERROR_DEVICE_HUNG, 0x887A_0006);
    assert_eq!(DXGI_ERROR_DEVICE_RESET, 0x887A_0007);
    assert_eq!(DXGI_ERROR_DRIVER_INTERNAL_ERROR, 0x887A_0020);
}

#[test]
fn a_lost_device_names_the_call_and_the_code() {
    let text = GpuError::from_hresult("Present", DXGI_ERROR_DEVICE_REMOVED).to_string();
    assert_eq!(
        text,
        "Present: the GPU device was lost (HRESULT 0x887a0005)"
    );
}

#[test]
fn a_spout_refusal_is_not_a_lost_device_and_names_the_sender() {
    let cases = [
        (
            GpuError::SpoutName {
                name: String::new(),
                reason: "empty",
            },
            "invalid Spout sender name \"\": empty",
        ),
        (
            GpuError::SpoutNameTaken {
                name: "SP-program-MAX".to_owned(),
            },
            "the Spout sender name \"SP-program-MAX\" is held by another sender",
        ),
        (
            GpuError::SpoutNotRegistered {
                name: "SP-program-MAX".to_owned(),
            },
            "Spout did not register the sender as \"SP-program-MAX\" (another sender took the name, or Spout's sender list is full)",
        ),
        (
            GpuError::Spout {
                call: "spout_sender_send",
                code: 3,
            },
            "spout_sender_send failed in Spout (code 0x3)",
        ),
    ];
    for (error, text) in cases {
        assert!(!error.is_device_lost(), "{error:?}");
        assert_eq!(error.to_string(), text);
    }
}
