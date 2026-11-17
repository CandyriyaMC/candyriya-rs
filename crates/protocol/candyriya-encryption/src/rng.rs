//! Bridge between rand's thread-local RNG (the workspace standard) and
//! rsa 0.10's fallible `TryCryptoRng` interface.
//!
//! rsa 0.10 types every RNG requirement as `TryCryptoRng`. rand 0.9's
//! thread RNG is infallible, so every operation delegates straight through
//! and the `Infallible` error type is preserved. Nothing is generated here
//! and there is no new source of entropy - this is a thin, auditable shim.
//!
//! This exists so the workspace does not need to opt into rsa's `getrandom`
//! feature (which would drag in getrandom 0.4, unbuildable on hosts without
//! the GNU import-library tools) while still passing proper blinded RNGs
//! into key generation and decryption.

use core::convert::Infallible;

use rand::RngCore;
use rsa::rand_core::{TryCryptoRng, TryRng};

pub(crate) struct RandThreadRng(rand::rngs::ThreadRng);

impl TryRng for RandThreadRng {
    type Error = Infallible;

    fn try_next_u32(&mut self) -> Result<u32, Self::Error> {
        Ok(self.0.next_u32())
    }

    fn try_next_u64(&mut self) -> Result<u64, Self::Error> {
        Ok(self.0.next_u64())
    }

    fn try_fill_bytes(&mut self, dst: &mut [u8]) -> Result<(), Self::Error> {
        rand::RngCore::fill_bytes(&mut self.0, dst);
        Ok(())
    }
}

impl TryCryptoRng for RandThreadRng {}

// callers need an owned handle so they can pass it as &mut down the call stack
pub(crate) fn thread_rng() -> RandThreadRng {
    RandThreadRng(rand::rng())
}
