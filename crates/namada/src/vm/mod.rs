//! Virtual machine modules for running transactions and validity predicates.

use std::marker::PhantomData;
use std::ptr::NonNull;

use wasmparser::{Validator, WasmFeatures};

pub mod host_env;
pub mod memory;
pub mod prefix_iter;
pub mod types;
#[cfg(feature = "wasm-runtime")]
pub mod wasm;
use thiserror::Error;

const UNTRUSTED_WASM_FEATURES: WasmFeatures = WasmFeatures {
    mutable_global: false,
    saturating_float_to_int: false,
    sign_extension: true,
    reference_types: false,
    multi_value: false,
    bulk_memory: false,
    simd: false,
    relaxed_simd: false,
    threads: false,
    tail_call: false,
    floats: true,
    multi_memory: false,
    exceptions: false,
    memory64: false,
    extended_const: false,
    component_model: false,
    function_references: false,
    memory_control: false,
    gc: false,
};

#[allow(missing_docs)]
#[derive(Error, Debug, Clone)]
pub enum WasmValidationError {
    #[error(
        "Invalid WASM using forbidden features: {0}. Expected: \
         {UNTRUSTED_WASM_FEATURES:?}"
    )]
    ForbiddenWasmFeatures(wasmparser::BinaryReaderError),
}

/// WASM Cache access level, used to limit dry-ran transactions to read-only
/// cache access.
pub trait WasmCacheAccess: Clone + std::fmt::Debug + Default {
    /// Is access read/write?
    fn is_read_write() -> bool;
}

/// Regular read/write caches access
#[derive(Debug, Clone, Default)]
pub struct WasmCacheRwAccess;
impl WasmCacheAccess for WasmCacheRwAccess {
    fn is_read_write() -> bool {
        true
    }
}

/// Restricted read-only access for dry-ran transactions
#[derive(Debug, Clone, Default)]
pub struct WasmCacheRoAccess;

impl WasmCacheAccess for WasmCacheRoAccess {
    fn is_read_write() -> bool {
        false
    }
}

/// Read-only access to host data.
pub enum RoAccess {}

/// Read and write access to host data.
pub enum RwAccess {}

/// Reference to host environment data, to be used from wasm
/// to implement host functions.
#[derive(Clone, Debug)]
pub struct HostRef<ACCESS, T> {
    data: NonNull<T>,
    _access: PhantomData<*const ACCESS>,
}

// TODO: ensure `T` is `Send` and `Sync`
unsafe impl<T> Send for HostRef<'_, T> {}
unsafe impl<T> Sync for HostRef<'_, T> {}

impl<T> HostRef<RoAccess, T> {
    /// Wrap a reference to the VM environment.
    ///
    /// # Safety
    ///
    /// The caller must ensure the reference to the VM environment
    /// is valid and non-null.
    pub unsafe fn new(host_structure: &T) -> Self {
        Self {
            data: NonNull::new_unchecked(host_structure as *mut _),
            _access: PhantomData,
        }
    }

    /// Get a reference from VM environment.
    ///
    /// # Safety
    ///
    /// The caller must ensure the reference to the VM environment
    /// is still valid.
    pub unsafe fn get<'a>(&self) -> &'a T {
        self.data.as_ref()
    }
}

impl<T> HostRef<RwAccess, T> {
    /// Wrap a mutable reference to the VM environment.
    ///
    /// # Safety
    ///
    /// The caller must ensure the reference to the VM environment
    /// is valid and non-null.
    pub unsafe fn new(host_structure: &mut T) -> Self {
        Self {
            data: NonNull::new_unchecked(host_structure as *mut _),
            _access: PhantomData,
        }
    }

    /// Get a reference from VM environment.
    ///
    /// # Safety
    ///
    /// The caller must ensure the reference to the VM environment
    /// is still valid.
    pub unsafe fn get<'a>(&self) -> &'a T {
        self.data.as_ref()
    }

    /// Get a mutable reference from VM environment.
    ///
    /// # Safety
    ///
    /// The caller must ensure the reference to the VM environment
    /// is still valid.
    pub unsafe fn get_mut<'a>(&self) -> &'a mut T {
        self.data.as_mut()
    }
}

/// Validate an untrusted wasm code with restrictions that we place such code
/// (e.g. transaction and validity predicates)
pub fn validate_untrusted_wasm(
    wasm_code: impl AsRef<[u8]>,
) -> Result<(), WasmValidationError> {
    let mut validator = Validator::new_with_features(UNTRUSTED_WASM_FEATURES);
    let _types = validator
        .validate_all(wasm_code.as_ref())
        .map_err(WasmValidationError::ForbiddenWasmFeatures)?;
    Ok(())
}
