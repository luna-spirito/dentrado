//! Test-only utilities shared by the crate's unit tests.
//!
//! The page-cache backend opens its store on the first storage call and keeps
//! the store's driver tasks on the thread's compio runtime, so a test's
//! storage must outlive every call that touches it. One runtime per test
//! thread — created lazily and dropped with the thread — gives every test its
//! own store namespace while letting helper functions block on their own
//! futures without tearing the store down in between.

use std::future::Future;

use compio::runtime::Runtime;

thread_local! {
    /// The test thread's runtime, shared by every `block_on` on this thread.
    static RUNTIME: Runtime = Runtime::new().expect("a compio runtime for the test thread");
}

/// Drive `fut` to completion on this test thread's runtime.
pub(crate) fn block_on<F: Future>(fut: F) -> F::Output {
    RUNTIME.with(|rt| rt.block_on(fut))
}
