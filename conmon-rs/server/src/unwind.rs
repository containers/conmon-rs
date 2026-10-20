//! Unwind stubs for the static s390x build.
//!
//! Workaround for rustc bug: https://github.com/rust-lang/rust/issues/47493
//!
//! It shouldn't even be possible to reach these functions, thanks to
//! panic=abort, but libcore is compiled with unwinding enabled and that ends up
//! making unreachable references to them.

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
