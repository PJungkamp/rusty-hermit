use core::fmt;

/// An ABI compatible [`Result`] type.
///
/// # Implementors
///
/// - Signed Integers: A positive value or zero indicates success and negative value failure.
pub trait AbiResult: Copy + Sized + fmt::Display {
	fn check(self) -> Result<OkVal<Self>, ErrVal<Self>>;
}

/// Wrapper around a [`AbiResult`] value which is guaranteed to represent [`Ok`].
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct OkVal<R: AbiResult>(R);

/// Wrapper around a [`AbiResult`] value which is guaranteed to represent [`Err`].
#[repr(transparent)]
#[derive(Debug, Clone, Copy)]
pub struct ErrVal<R: AbiResult>(R);

/// Invalid [`AbiResult`] value error.
#[derive(Debug, Clone, Copy)]
pub struct InvalidValueError<R: AbiResult> {
	pub value: R,
}

/// Convertible to [`OkVal`].
pub trait AsOkVal<R: AbiResult> {
	fn as_ok(&self) -> OkVal<R>;
}

/// Convertible to [`ErrVal`].
pub trait AsErrVal<R: AbiResult> {
	fn as_err(&self) -> ErrVal<R>;
}

/// Convertible to an [`AbiResult`].
///
/// # Implemetation - [`Result<T,E>`]
///
/// A [`Result`] will try to convert an [`Ok`] by calling [`AsOkVal::as_ok`]
/// and an [`Err`] by calling [`AsErrVal::as_err`] when traits are satisfied.
///
/// # Example - [`Result<isize,isize>`]
///
/// These conversions are fine:
/// ```
/// # use hermit_util::abi::AsAbiResult;
/// let _: isize = Result::<isize,isize>::Ok(1).as_abi();
/// let _: isize = Result::<isize,isize>::Err(-1).as_abi();
/// ```
///
/// While these will panic:
/// ```should_panic
/// # use hermit_util::abi::AsAbiResult;
/// let _: isize = Result::<isize,isize>::Err(1).as_abi();
/// let _: isize = Result::<isize,isize>::Ok(-1).as_abi();
/// ```
pub trait AsAbiResult<R: AbiResult> {
	/// Convert to [`AbiResult`].
	///
	/// # Panics
	///
	/// This function may panic if the provided value is not representable as the [`AbiResult`] type.
	fn as_abi(&self) -> R;
}

/// Convertible from [`OkVal`].
pub trait TryFromOkVal<R: AbiResult>: Sized {
	fn try_from_ok(ok: OkVal<R>) -> Result<Self, InvalidValueError<R>>;
}

/// Convertible from [`ErrVal`].
pub trait TryFromErrVal<R: AbiResult>: Sized {
	fn try_from_err(err: ErrVal<R>) -> Result<Self, InvalidValueError<R>>;
}

/// Convertible from [`AbiResult`].
pub trait TryFromAbiResult<R: AbiResult>: Sized {
	fn try_from_abi(result: R) -> Result<Self, InvalidValueError<R>>;
}

impl<R: AbiResult> OkVal<R> {
	/// Create new [`OkVal`] if the value is non-negative.
	pub fn new(value: R) -> Option<Self> {
		value.check().is_ok().then(|| Self(value))
	}

	/// Create new [`OkVal`] without bounds check.
	///
	/// # Safety
	///
	/// The value must be greater than or equal to 0.
	pub unsafe fn new_unchecked(value: R) -> Self {
		Self(value)
	}

	#[must_use]
	/// Get the inner value.
	pub fn get(self) -> R {
		self.0
	}
}

impl<R: AbiResult> ErrVal<R> {
	/// Create new [`ErrVal`] if the value is negative.
	pub fn new(value: R) -> Option<Self> {
		value.check().is_err().then(|| Self(value))
	}

	/// Create new [`ErrVal`] without bounds check.
	///
	/// # Safety
	///
	/// The value must be smaller than 0.
	pub unsafe fn new_unchecked(value: R) -> Self {
		Self(value)
	}

	/// Get the inner value.
	#[must_use]
	pub fn get(self) -> R {
		self.0
	}
}

impl<R: AbiResult + fmt::Display> fmt::Display for InvalidValueError<R> {
	fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
		let Self { value } = self;
		match value.check() {
			Ok(OkVal(value)) => write!(f, "Invalid result: Ok({value})"),
			Err(ErrVal(value)) => write!(f, "Invalid result: Err({value})"),
		}
	}
}

// -------------------------------------------------------
// Implementations for ::core types
// -------------------------------------------------------

impl<R, T, E> TryFromAbiResult<R> for Result<T, E>
where
	R: AbiResult,
	T: TryFromOkVal<R>,
	E: TryFromErrVal<R>,
{
	fn try_from_abi(value: R) -> Result<Self, InvalidValueError<R>> {
		Ok(match value.check() {
			Ok(ok) => Ok(T::try_from_ok(ok)?),
			Err(err) => Err(E::try_from_err(err)?),
		})
	}
}

impl<R, T, E> AsAbiResult<R> for Result<T, E>
where
	R: AbiResult,
	T: AsOkVal<R>,
	E: AsErrVal<R>,
{
	fn as_abi(&self) -> R {
		match self {
			Ok(t) => t.as_ok().get(),
			Err(e) => e.as_err().get(),
		}
	}
}

impl<R: AbiResult> AsOkVal<R> for R {
	fn as_ok(&self) -> OkVal<R> {
		OkVal::new(*self).unwrap_or_else(|| panic!("Value {self} is not a valid OkVal"))
	}
}

impl<R: AbiResult> TryFromOkVal<R> for R {
	fn try_from_ok(ok: OkVal<R>) -> Result<Self, InvalidValueError<R>> {
		Ok(ok.get())
	}
}

impl<R: AbiResult> AsErrVal<R> for R {
	fn as_err(&self) -> ErrVal<R> {
		ErrVal::new(*self).unwrap_or_else(|| panic!("Value {self} is not a valid ErrVal"))
	}
}

impl<R: AbiResult> TryFromErrVal<R> for R {
	fn try_from_err(err: ErrVal<R>) -> Result<Self, InvalidValueError<R>> {
		Ok(err.get())
	}
}

macro_rules! impl_signed {
	( $( $signed:ty )* ) => {
		$(
			impl AbiResult for $signed {
				fn check(self) -> Result<OkVal<Self>,ErrVal<Self>> {
					if self.is_negative() {
						Err(ErrVal(self))
					} else {
						Ok(OkVal(self))
					}
				}
			}
		)*
	};
}

impl_signed! { i8 i16 i32 i64 isize }

macro_rules! impl_unsigned_ok {
	( $( $unsigned:ty as $signed:ty )* ) => {
		$(
			impl AsOkVal<$signed> for $unsigned {
				fn as_ok(&self) -> OkVal<$signed> {
					(*self as $signed).as_ok()
				}
			}

			impl TryFromOkVal<$signed> for $unsigned {
				fn try_from_ok(ok: OkVal<$signed>) -> Result<Self, InvalidValueError<$signed>> {
					Ok(ok.get() as $unsigned)
				}
			}
		)*
	};
}

impl_unsigned_ok! {
	u8 as i8
	u16 as i16
	u32 as i32
	u64 as i64
	usize as isize
}

macro_rules! impl_transmute_ok {
	( $( $ty:ty as $signed:ty )* ) => {
		$(
			impl AsOkVal<$signed> for $ty {
				fn as_ok(&self) -> OkVal<$signed> {
					let signed: $signed = unsafe { core::mem::transmute(*self) };
					signed.as_ok()
				}
			}

			impl TryFromOkVal<$signed> for $ty {
				fn try_from_ok(ok: OkVal<$signed>) -> Result<Self, InvalidValueError<$signed>> {
					let ty: $ty = unsafe { core::mem::transmute(ok.get()) };
					Ok(ty)
				}
			}
		)*
	};
}

impl_transmute_ok! {
	Option<core::num::NonZeroI8> as i8
	Option<core::num::NonZeroI16> as i16
	Option<core::num::NonZeroI32> as i32
	Option<core::num::NonZeroI64> as i64
	Option<core::num::NonZeroIsize> as isize
	Option<core::num::NonZeroU8> as i8
	Option<core::num::NonZeroU16> as i16
	Option<core::num::NonZeroU32> as i32
	Option<core::num::NonZeroU64> as i64
	Option<core::num::NonZeroUsize> as isize
}

macro_rules! impl_pointer_ok {
	( $generics:ident => $( $pointer:ty )* ) => {
		$(
			impl<$generics> AsOkVal<isize> for $pointer {
				fn as_ok(&self) -> OkVal<isize> {
					(*self as isize).as_ok()
				}
			}

			impl<$generics> TryFromOkVal<isize> for $pointer {
				fn try_from_ok(ok: OkVal<isize>) -> Result<Self, InvalidValueError<isize>> {
					Ok(ok.get() as $pointer)
				}
			}
		)*
	};
}

impl_pointer_ok! { T =>
	*const T
	*mut T
}

impl<T> AsOkVal<isize> for Option<core::ptr::NonNull<T>> {
	fn as_ok(&self) -> OkVal<isize> {
		let signed: isize = unsafe { core::mem::transmute(*self) };
		signed.as_ok()
	}
}

impl<T> TryFromOkVal<isize> for Option<core::ptr::NonNull<T>> {
	fn try_from_ok(ok: OkVal<isize>) -> Result<Self, InvalidValueError<isize>> {
		let ptr = unsafe { core::mem::transmute(ok.get()) };
		Ok(ptr)
	}
}
