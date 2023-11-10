use std::{error::Error, ffi::c_char, fmt::Debug};

use post::{
    config::ScryptParams,
    initialize::{CpuInitializer, Initialize},
    pos_verification::VerificationError,
};
use scrypt_ocl::{ocl::DeviceType, OpenClInitializer, ProviderId};

use crate::post_impl::VerifyResult;

pub enum Initializer {}

struct InitializerWrapper {
    inner: Box<dyn Initialize>,
    commitment: [u8; 32],
    vrf_difficulty: Option<[u8; 32]>,
}

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[allow(clippy::enum_variant_names)]
pub enum InitializeResult {
    InitializeOk = 0,
    InitializeOkNonceNotFound = 1,
    InitializeInvalidLabelsRange = 2,
    InitializeError = 3,
    InitializeInvalidArgument = 4,
    InitializeFailedToGetProviders = 5,
}

#[repr(C)]
#[derive(Clone, PartialEq, Eq)]
pub struct Provider {
    name: [c_char; 64],
    id: u32,
    class: DeviceClass,
}

pub const CPU_PROVIDER_ID: u32 = u32::MAX;

#[repr(C)]
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
#[allow(clippy::upper_case_acronyms)]
pub enum DeviceClass {
    CPU = 1,
    GPU = 2,
}

/// Returns the number of providers available.
#[no_mangle]
pub extern "C" fn get_providers_count() -> usize {
    // Add one for the CPU provider.
    scrypt_ocl::get_providers_count(Some(DeviceType::GPU)) + 1
}

/// Returns all available providers.
#[no_mangle]
pub extern "C" fn get_providers(out: *mut Provider, out_len: usize) -> InitializeResult {
    if out.is_null() {
        log::error!("out is null");
        return InitializeResult::InitializeInvalidArgument;
    }

    let providers = match scrypt_ocl::get_providers(Some(DeviceType::GPU)) {
        Ok(providers) => providers,
        Err(e) => {
            log::error!("failed to get providers: {e}");
            return InitializeResult::InitializeFailedToGetProviders;
        }
    };

    let out = unsafe { std::slice::from_raw_parts_mut(out, out_len) };

    let mut id = 0;
    for (out, provider) in out.iter_mut().zip(providers.iter()) {
        // Copy over the first out.name.len() - 1 bytes, and then add a null terminator.
        let name = format!("{provider}")
            .bytes()
            .map(|b| b as c_char)
            .take(out.name.len() - 1)
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        out.name[..name.len()].copy_from_slice(&name);
        out.id = id as u32;
        out.class = DeviceClass::GPU;
        id += 1;
    }
    if id < out.len() {
        out[id] = Provider {
            name: [0; 64],
            id: CPU_PROVIDER_ID,
            class: DeviceClass::CPU,
        };
        let name = b"[CPU] scrypt-jane\0";
        out[id].name[..name.len()].copy_from_slice(&name.map(|b| b as c_char));
    }

    InitializeResult::InitializeOk
}

/// Initializes labels for the given range.
///
/// start and end are inclusive.
#[no_mangle]
pub extern "C" fn initialize(
    initializer: *mut Initializer,
    start: u64,
    end: u64,
    out_buffer: *mut u8,
    out_nonce: *mut u64,
) -> InitializeResult {
    // Convert end to exclusive
    if end == u64::MAX {
        log::error!("end must be < u64::MAX");
        return InitializeResult::InitializeInvalidLabelsRange;
    }
    let end = end + 1;

    let initializer = unsafe { &mut *(initializer as *mut InitializerWrapper) };
    let len = match usize::try_from(end - start) {
        Ok(len) => len * 16,
        Err(e) => {
            log::error!("failed to calculate number of labels to initialize: {e}");
            return InitializeResult::InitializeInvalidLabelsRange;
        }
    };

    let mut labels = unsafe { std::slice::from_raw_parts_mut(out_buffer, len) };
    let vrf_nonce = match initializer.inner.initialize_to(
        &mut labels,
        &initializer.commitment,
        start..end,
        initializer.vrf_difficulty,
    ) {
        Ok(nonce) => nonce,
        Err(e) => {
            log::error!("error initializing labels: {e:?}");
            return InitializeResult::InitializeError;
        }
    };

    if !out_nonce.is_null() {
        if let Some(nonce) = vrf_nonce {
            unsafe { *out_nonce = nonce.index };
        } else {
            return InitializeResult::InitializeOkNonceNotFound;
        }
    }
    InitializeResult::InitializeOk
}

#[no_mangle]
pub extern "C" fn new_initializer(
    provider_id: u32,
    n: usize,
    commitment: *const u8,
    vrf_difficulty: *const u8,
) -> *mut Initializer {
    match _new_initializer(provider_id, n, commitment, vrf_difficulty) {
        Ok(initializer) => Box::into_raw(initializer) as _,
        Err(e) => {
            log::error!("error creating initializer: {e:?}");
            std::ptr::null_mut()
        }
    }
}

fn _new_initializer(
    provider_id: u32,
    n: usize,
    commitment: *const u8,
    vrf_difficulty: *const u8,
) -> Result<Box<InitializerWrapper>, Box<dyn Error>> {
    if !n.is_power_of_two() {
        return Err("scrypt N must be a power of two".into());
    }
    let commitment = unsafe { std::slice::from_raw_parts(commitment, 32) };
    let commitment = commitment.try_into()?;

    let vrf_difficulty = if vrf_difficulty.is_null() {
        None
    } else {
        let vrf_difficulty = unsafe { std::slice::from_raw_parts(vrf_difficulty, 32) };
        Some(vrf_difficulty.try_into()?)
    };

    let instance: Box<dyn Initialize> = match provider_id {
        CPU_PROVIDER_ID => Box::new(CpuInitializer::new(ScryptParams::new(n, 1, 1))),
        id => Box::new(OpenClInitializer::new(
            Some(ProviderId(id)),
            n,
            Some(DeviceType::GPU),
        )?),
    };
    let initializer = Box::new(InitializerWrapper {
        inner: instance,
        commitment,
        vrf_difficulty,
    });

    Ok(initializer)
}

#[no_mangle]
pub extern "C" fn free_initializer(initializer: *mut Initializer) {
    unsafe { drop(Box::from_raw(initializer as *mut InitializerWrapper)) };
}

#[no_mangle]
pub extern "C" fn verify_pos(
    datadir: *const c_char,
    from_file: *const u32,
    to_file: *const u32,
    fraction: f64,
    scrypt: ScryptParams,
) -> VerifyResult {
    let datadir = unsafe { std::ffi::CStr::from_ptr(datadir) };
    let datadir = match datadir.to_str() {
        Ok(s) => s,
        Err(e) => {
            log::error!("invalid datadir: {e}");
            return VerifyResult::Failed;
        }
    };
    let from_file = unsafe { from_file.as_ref() }.map(|f| *f as usize);
    let to_file = unsafe { to_file.as_ref() }.map(|f| *f as usize);

    match post::pos_verification::verify_files(
        std::path::Path::new(datadir),
        fraction,
        from_file,
        to_file,
        scrypt,
    ) {
        Ok(_) => VerifyResult::Ok,
        Err(VerificationError::InvalidLabel { idx, offset }) => {
            log::info!(
                "POS data is invalid: {}",
                VerificationError::InvalidLabel { idx, offset }
            );
            VerifyResult::Invalid
        }
        Err(e) => {
            log::error!("Error verifying POS data: {e:?}");
            VerifyResult::Failed
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        ffi::CString,
        ptr::{null, null_mut},
    };

    use post::{
        config::ScryptParams,
        initialize::{CpuInitializer, Initialize, MockInitialize},
    };
    use tempfile::tempdir;

    use crate::{
        initialization::{Initializer, InitializerWrapper},
        post_impl::VerifyResult,
    };

    use super::{verify_pos, InitializeResult, CPU_PROVIDER_ID};

    #[test]
    fn cant_initialize_more_than_2_64_labels() {
        let initializer = super::new_initializer(CPU_PROVIDER_ID, 32, [0u8; 32].as_ptr(), null());

        let mut labels = Vec::new();
        let result = super::initialize(initializer, 0, u64::MAX, labels.as_mut_ptr(), null_mut());
        assert_eq!(InitializeResult::InitializeInvalidLabelsRange, result);
    }

    #[test]
    fn initialization() {
        let indices = 0..=70;

        let initializer = super::new_initializer(CPU_PROVIDER_ID, 32, [0u8; 32].as_ptr(), null());

        let mut labels = vec![0u8; 71 * 16];
        let result = super::initialize(
            initializer,
            *indices.start(),
            *indices.end(),
            labels.as_mut_ptr(),
            null_mut(),
        );
        assert_eq!(InitializeResult::InitializeOk, result);

        let mut expected = Vec::<u8>::with_capacity(indices.clone().count());

        CpuInitializer::new(ScryptParams::new(32, 1, 1))
            .initialize_to(
                &mut expected,
                &[0u8; 32],
                *indices.start()..*indices.end() + 1,
                None,
            )
            .unwrap();

        assert_eq!(expected, labels);

        super::free_initializer(initializer);
    }

    #[test]
    fn initialization_failure() {
        let mut init_mock = Box::new(MockInitialize::new());
        init_mock
            .expect_initialize_to()
            .once()
            .returning(|_, _, _, _| Err("error".into()));

        let mut initializer = Box::new(InitializerWrapper {
            inner: init_mock,
            commitment: [0u8; 32],
            vrf_difficulty: None,
        });

        let result = super::initialize(
            initializer.as_mut() as *mut InitializerWrapper as *mut Initializer,
            0,
            99,
            null_mut(),
            null_mut(),
        );
        assert_eq!(InitializeResult::InitializeError, result);
    }

    #[test]
    fn cpu_provider_is_always_available() {
        let initializer = super::new_initializer(CPU_PROVIDER_ID, 32, [0u8; 32].as_ptr(), null());
        assert!(!initializer.is_null());
        super::free_initializer(initializer);
    }

    #[test]
    fn free_gpu_initializer() {
        let initializer = super::new_initializer(0, 32, [0u8; 32].as_ptr(), null());
        if !initializer.is_null() {
            super::free_initializer(initializer);
        }
    }

    #[test]
    fn initialization_nonce_not_found() {
        let indices = 0..=0;

        let initializer =
            super::new_initializer(CPU_PROVIDER_ID, 32, [0; 32].as_ptr(), [0; 32].as_ptr());

        let mut labels = vec![0u8; 16];
        let mut nonce = 0xCAFEDEAD;
        let result = super::initialize(
            initializer,
            *indices.start(),
            *indices.end(),
            labels.as_mut_ptr(),
            &mut nonce,
        );
        assert_eq!(InitializeResult::InitializeOkNonceNotFound, result);
        assert_eq!(0xCAFEDEAD, nonce);
    }

    #[test]
    fn initialization_nonce_found() {
        let indices = 0..=0;

        let initializer =
            super::new_initializer(CPU_PROVIDER_ID, 32, [0; 32].as_ptr(), [0xFF; 32].as_ptr());

        let mut labels = vec![0u8; 16];
        let mut nonce = 0xCAFEDEAD;
        let result = super::initialize(
            initializer,
            *indices.start(),
            *indices.end(),
            labels.as_mut_ptr(),
            &mut nonce,
        );
        assert_eq!(InitializeResult::InitializeOk, result);
        assert_ne!(0xCAFEDEAD, nonce);
    }

    #[test]
    fn get_providers_count() {
        let count = super::get_providers_count();
        assert!(dbg!(count) > 0);
    }

    #[test]
    fn get_providers() {
        let count = super::get_providers_count();
        let mut providers = vec![
            super::Provider {
                name: [0; 64],
                id: 0,
                class: super::DeviceClass::CPU
            };
            count
        ];

        assert_eq!(
            InitializeResult::InitializeOk,
            super::get_providers(providers.as_mut_ptr(), count)
        );
    }

    #[test]
    fn initialize_and_verify() {
        // Initialize some data
        let datadir = tempdir().unwrap();
        let scrypt = ScryptParams::new(2, 1, 1);

        CpuInitializer::new(scrypt)
            .initialize(datadir.path(), &[0u8; 32], &[0u8; 32], 256, 31, 700, None)
            .unwrap();

        // Verify the data
        let datapath = CString::new(datadir.path().to_str().unwrap()).unwrap();
        let result = verify_pos(datapath.as_ptr(), null(), null(), 100.0, scrypt);
        assert_eq!(VerifyResult::Ok, result);

        // verify with wrong scrypt params
        let wrong_scrypt = ScryptParams::new(4, 1, 1);
        let result = verify_pos(datapath.as_ptr(), null(), null(), 100.0, wrong_scrypt);
        assert_eq!(VerifyResult::Invalid, result);

        // verify with non-existent path
        let path = CString::new("non-existent-path").unwrap();
        let result = verify_pos(path.as_ptr(), null(), null(), 100.0, scrypt);
        assert_eq!(VerifyResult::Failed, result);
    }
}
