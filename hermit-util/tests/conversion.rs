use hermit_util::abi::{AsAbiResult, TryFromAbiResult};

mod error;

use error::Error;

#[test]
fn result_to_signed() {
	let result: Result<isize, Error> = Err(Error::ENOSYS);
	let integer = result.as_abi();
	assert_eq!(integer, -1);

	let result: Result<isize, Error> = Ok(15);
	let integer = result.as_abi();
	assert_eq!(integer, 15);
}

#[test]
fn last_valid_err_no() {
	let result: Result<isize, Error> = Err(Error::Valid);
	let _ = result.as_abi();
}
#[test]
#[should_panic]
fn invalid_err_no() {
	let result: Result<isize, Error> = Err(Error::Invalid);
	let _ = result.as_abi();
}

#[test]
#[should_panic]
fn invalid_err_val() {
	let result: Result<isize, isize> = Err(1);
	let _ = result.as_abi();
}

#[test]
#[should_panic]
fn invalid_ok_val() {
	let result: Result<isize, Error> = Ok(-1);
	let _ = result.as_abi();
}

#[test]
fn signed_to_result() {
	let integer = -2isize;
	let result = Result::<isize, Error>::try_from_abi(integer).unwrap();
	assert_eq!(result, Err(Error::ENOMEM));

	let integer = 13isize;
	let result = Result::<isize, Error>::try_from_abi(integer).unwrap();
	assert_eq!(result, Ok(13));

	// 4 is not a member of Error
	let integer = -4isize;
	assert!(Result::<isize, Error>::try_from_abi(integer).is_err());
}

fn fallible_computation() -> Result<isize, Error> {
	Err(Error::ENOMEM)
}

#[no_mangle]
extern "C" fn c_fallible_computation() -> isize {
	fallible_computation().as_abi()
}

#[test]
fn compare_c_and_rust_call() {
	let result = fallible_computation();
	let c_result = c_fallible_computation();
	assert_eq!(result, Result::try_from_abi(c_result).unwrap());
}

#[test]
fn transparent_repr() {
	extern "C" {
		fn c_fallible_computation() -> isize;
	}
	unsafe {
		assert_eq!(c_fallible_computation(), -(Error::ENOMEM as isize));
	}
}

#[test]
fn c_wrapper() {
	fn fallible_computation_wrapper() -> Result<isize, Error> {
		Result::try_from_abi(c_fallible_computation()).unwrap()
	}
	assert_eq!(fallible_computation_wrapper(), Err(Error::ENOMEM));
}
