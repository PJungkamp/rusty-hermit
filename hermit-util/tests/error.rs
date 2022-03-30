use hermit_util::derive::ErrNo;

pub mod errno {
	pub const ENOSYS: isize = 1;
	pub const ENOMEM: isize = 2;
}

/// Works for the default isize discriminants
#[derive(Debug, ErrNo, Clone, Copy, PartialEq, Eq)]
pub enum Error {
	ENOSYS = errno::ENOSYS,
	ENOMEM = errno::ENOMEM,
	// greatest valid value for an ErrVal
	Valid = isize::MIN,
	// D is now too large to fit the ErrVal
	Invalid,
}

/// Works for an explicit `#[repr(…)]`
#[repr(i16)]
#[derive(Debug, ErrNo, Clone, Copy, PartialEq, Eq)]
pub enum ErrorI16 {
	ENOSYS = errno::ENOSYS as i16,
	ENOMEM = errno::ENOMEM as i16,
}
