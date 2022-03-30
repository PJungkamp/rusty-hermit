#![no_std]

/// ABI related utilities
pub mod abi;
/// Derive macros
pub mod derive {
	/// Derives the [`TryFromErr`](crate::abi::TryFromErrVal) and [`AsErr`](crate::abi::AsErrVal)
	/// traits on a descriminated C-style enum.
	pub use hermit_macros::ErrNo;
}

mod sealed {
	pub trait Sealed {}
}
