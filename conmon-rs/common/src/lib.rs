pub mod conmon_capnp {
    #![allow(clippy::all)]
    include!(concat!(env!("OUT_DIR"), "/proto/conmon_capnp.rs"));
}

/// Workaround for rustc bug: https://github.com/rust-lang/rust/issues/47493
///
/// It shouldn't even be possible to reach this function, thanks to panic=abort,
/// but libcore is compiled with unwinding enabled and that ends up making unreachable
/// references to this.
#[cfg(all(target_os = "linux", target_arch = "s390x", target_env = "musl"))]
mod unwind {
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_Backtrace() {
        unimplemented!("_Unwind_Backtrace")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_DeleteException() {
        unimplemented!("_Unwind_DeleteException")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_GetDataRelBase() {
        unimplemented!("_Unwind_GetDataRelBase")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_GetIP() {
        unimplemented!("_Unwind_GetIP")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_GetIPInfo() {
        unimplemented!("_Unwind_GetIPInfo")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_GetLanguageSpecificData() {
        unimplemented!("_Unwind_GetLanguageSpecificData")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_GetRegionStart() {
        unimplemented!("_Unwind_GetRegionStart")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_GetTextRelBase() {
        unimplemented!("_Unwind_GetTextRelBase")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_RaiseException() {
        unimplemented!("_Unwind_RaiseException")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_Resume() {
        unimplemented!("_Unwind_Resume")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_SetGR() {
        unimplemented!("_Unwind_SetGR")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_SetIP() {
        unimplemented!("_Unwind_SetIP")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_FindEnclosingFunction() {
        unimplemented!("_Unwind_FindEnclosingFunction")
    }
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_GetCFA() {
        unimplemented!("_Unwind_GetCFA")
    }
    #[cfg(target_arch = "arm")]
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_VRS_Get() {
        unimplemented!("_Unwind_VRS_Get")
    }
    #[cfg(target_arch = "arm")]
    #[unsafe(no_mangle)]
    unsafe extern "C" fn _Unwind_VRS_Set() {
        unimplemented!("_Unwind_VRS_Set")
    }
    #[cfg(target_arch = "arm")]
    #[unsafe(no_mangle)]
    unsafe extern "C" fn __aeabi_unwind_cpp_pr0() {
        unimplemented!("__aeabi_unwind_cpp_pr0")
    }
    #[cfg(target_arch = "arm")]
    #[unsafe(no_mangle)]
    unsafe extern "C" fn __aeabi_unwind_cpp_pr1() {
        unimplemented!("__aeabi_unwind_cpp_pr1")
    }
    #[cfg(target_arch = "arm")]
    #[unsafe(no_mangle)]
    unsafe extern "C" fn __gnu_unwind_frame() {
        unimplemented!("__gnu_unwind_frame")
    }
}
