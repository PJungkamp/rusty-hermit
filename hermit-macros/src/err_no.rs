use proc_macro2::{Ident, TokenStream};
use quote::{quote, ToTokens};
use syn::{
	parse_quote, punctuated::Punctuated, token::Comma, Data, DataEnum, DeriveInput, Error, Result,
	Type, Variant,
};

type Variants = Punctuated<Variant, Comma>;

fn get_enum(input: DeriveInput) -> Result<(Ident, Type, Variants)> {
	let DeriveInput {
		attrs, ident, data, ..
	} = input;

	let repr = attrs
		.into_iter()
		.find(|attr| attr.path.is_ident("repr"))
		.map(|attr| attr.parse_args::<Type>())
		.unwrap_or_else(|| Ok(parse_quote!(isize)))?;

	let DataEnum { variants, .. } = if let Data::Enum(data) = data {
		data
	} else {
		return Err(Error::new(
			ident.span(),
			"can only derive ErrNo for discriminanted C-style enums",
		));
	};

	Ok((ident, repr, variants))
}

pub fn expand(input: DeriveInput) -> Result<TokenStream> {
	let (enum_ident, repr, variants) = get_enum(input)?;
	let util = quote! { ::hermit_util::abi };
	
	let (from_enum_match, from_err_val_match): (Vec<_>, Vec<_>) = variants
		.iter()
		.map(|variant| {
			let Variant { ident, .. } = variant;
			let panic_message = format!("Discriminant of {enum_ident}::{ident} is not a valid ErrVal");

			let from_enum_match_arm = quote! {
				variant @ #enum_ident::#ident => #util::ErrVal::<#repr>::new(#repr::wrapping_sub(0,#enum_ident::#ident as #repr))
					.expect(#panic_message)			
			}
			.into_token_stream();

			let from_err_val_match_arm = quote! {
				err_val
					if err_val.get().abs() == #enum_ident::#ident as #repr =>
				Ok(Self::#ident)
			}
			.into_token_stream();

			Ok((from_enum_match_arm, from_err_val_match_arm))
		})
		.collect::<Result<Vec<_>>>()?
		.into_iter()
		.unzip();

	Ok(quote! {
		impl #util::AsErrVal<#repr> for #enum_ident {
			fn as_err(&self) -> #util::ErrVal<#repr> {
				match self {
					#(#from_enum_match,)*
				}
			}
		}

		impl #util::TryFromErrVal<#repr> for #enum_ident {
			fn try_from_err(err: #util::ErrVal<#repr>) -> Result<Self,#util::InvalidValueError<#repr>> {
				match err {
					#(#from_err_val_match,)*
					invalid => Err(#util::InvalidValueError {
						value: invalid.get()
					}),
				}
			}
		}
	})
}
