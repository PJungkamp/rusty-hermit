use proc_macro::TokenStream;
use syn::{parse_macro_input, DeriveInput, Error};

mod err_no;

#[proc_macro_derive(ErrNo)]
pub fn derive_err_no(input: TokenStream) -> TokenStream {
	let input = parse_macro_input!(input as DeriveInput);
	err_no::expand(input)
		.unwrap_or_else(Error::into_compile_error)
		.into()
}
