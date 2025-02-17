#![feature(proc_macro_diagnostic)]

use proc_macro::TokenStream;
use quote::quote;
use syn::parse::{Parse, ParseStream, Result};
use syn::punctuated::Punctuated;
use syn::spanned::Spanned;
use syn::visit_mut::VisitMut;
use syn::*;
mod elision;

/// AST of an iterator item. Similar to an `Item::Fn`
///
/// We *could* use an `Fn` directly here, and get parsing from it, but given the objective of this
/// crate is to explore the syntactic space, doing all of the parsing ourselves seems like a better
/// approach.
struct IteratorItemParse {
    attributes: Vec<Attribute>,
    visibility: Visibility,
    is_async: bool,
    name: Ident,
    generics: Generics,
    args: Punctuated<FnArg, Token![,]>,
    yields: Option<Type>,
    body: Block,
}

impl Parse for IteratorItemParse {
    /// Hi! If you are looking to hack on this crate to come up with your own syntax, **look here**!
    fn parse(input: ParseStream) -> Result<Self> {
        // This will parse the following:
        // `#[attr(..)] #[attr2] pub async fn* foo(<args>) yields Ty { ... }`
        let attributes: Vec<Attribute> = input.call(Attribute::parse_outer)?;
        let visibility: Visibility = input.parse()?;
        let r#async: Option<Token![async]> = input.parse()?;
        input.parse::<Token![fn]>()?;
        input.parse::<Token![*]>()?;
        let name: Ident = input.parse()?;
        let generics: Generics = input.parse()?;
        let fn_args;
        parenthesized!(fn_args in input);
        let args = parse_fn_args(&fn_args)?;
        let yields: Option<Ident> = input.parse()?;
        let yields: Option<Type> = if let Some(yields) = yields {
            if yields != "yields" {
                yields
                    .span()
                    .unwrap()
                    .error("expected contextual keyword `yields` or the start of an iterator body")
                    .emit();
                // FIXME: potentially deal better with this and try to recover the parse in a way
                // that doesn't spam an user that forgot to write yields or tried to write `->`.
            }
            Some(input.parse()?)
        } else {
            None
        };
        let body: Block = input.parse()?;
        Ok(IteratorItemParse {
            attributes,
            visibility,
            is_async: r#async.is_some(),
            name,
            generics,
            args,
            yields,
            body,
        })
    }
}

impl IteratorItemParse {
    fn build(self) -> TokenStream {
        let IteratorItemParse {
            mut attributes,
            visibility,
            is_async,
            name,
            mut generics,
            args,
            yields,
            mut body,
        } = self;
        let yields = match yields {
            Some(ty) => ty,
            None => Type::Tuple(TypeTuple {
                paren_token: syn::token::Paren::default(),
                elems: Punctuated::new(),
            }),
        };
        let args = elision::unelide_lifetimes(&mut generics.params, args);
        let lifetimes: Vec<syn::Lifetime> =
            generics.lifetimes().map(|l| l.lifetime.clone()).collect();

        let mut visitor = Visitor::new(is_async);
        visitor.visit_block_mut(&mut body);
        let mut size_hint = quote!((0, None));
        attributes.retain(|attr| {
            // An annotation of the type `#[size_hint((0, None))] fn* foo() { ... }` lets the end
            // user provide code to override the default return of `Iterator::size_hint`.
            // FIXME: verify if an alternative name should be considered.
            // Once we do this is in the compiler, we can observe the materialized types of all the
            // arguments, *and* thier uses, so that for simpler cases where iterators are being
            // consumed once and without nesting, we can come up with an accurate `size_hint` (or
            // at least as accurate as the `size_hint()` call is for the inputs).
            // FIXME: we can do some of the above by modifying `Visitor` to keep track of renames
            // and reassigns of the input bindings and of them being iterated on in for loops, but
            // this will be tricky to get right.
            if attr.path.get_ident().map(|a| a.to_string()).as_deref() == Some("size_hint") {
                size_hint = attr.tokens.clone();
                // We are removing the attribute from the desugaring because we are parsing it
                // directly.
                false
            } else {
                true
            }
        });

        // The `yield panic!()` in the desugaring is to allow an empty body in the input to still
        // expand to a generator. `rustc` relies on the presence of a `yield` statement in a
        // closure body to turn it into a generator.
        let tail = quote! {
            #[allow(unreachable_code)]
            {
                return;
                yield panic!();
            }
        };
        let return_type = if is_async {
            // Whey don't we use `std`'s `Stream` here?
            // `Stream` is currently on the process of being reworked into `AsyncIterator`[1],
            // leveraging associated `async fn` support that isn't yet in nightly. For now, we
            // just rely on the library that people are actually using, the futures' crate Stream.
            // [1]: https://rust-lang.github.io/wg-async-foundations/vision/roadmap/async_iter/traits.html
            // quote! { impl ::core::stream::Stream<Item = #yields> #(+ #lifetimes)* }
            quote!(impl ::futures::stream::Stream<Item = #yields> #(+ #lifetimes)*)
        } else {
            quote!(impl ::core::iter::Iterator<Item = #yields> #(+ #lifetimes)*)
        };
        let expansion = if is_async {
            quote!(::iterator_item::__internal::AsyncIteratorItem { gen, size_hint })
        } else {
            quote!(::iterator_item::__internal::IteratorItem { gen, size_hint })
        };
        let head = if is_async {
            quote!(static move |mut __stream_ctx|)
        } else {
            quote!(move ||)
        };
        let args: Vec<_> = args.into_iter().collect();
        // Consider modifying this so that `gen` is `let gen = Box::pin(gen);`
        let expanded = quote! {
            #(#attributes)* #visibility fn #name #generics(#(#args),*) -> #return_type {
                #[allow(unused_parens)]
                let size_hint = #size_hint;
                let gen = #head {
                    #body
                    #tail
                };
                #expansion
            }
        };

        TokenStream::from(expanded)
    }
}

#[proc_macro]
pub fn iterator_item(input: TokenStream) -> TokenStream {
    let item: IteratorItemParse = parse_macro_input!(input as IteratorItemParse);
    item.build()
}

/// This `Visitor` allows us to modify the body (block) of the parsed item to make changes to it
/// before passing it back to `rustc`. This allows us to construct our own desugaring for `await`
/// and `yield`.
struct Visitor {
    is_async: bool,
}

impl Visitor {
    fn new(is_async: bool) -> Self {
        Visitor { is_async }
    }
}

impl VisitMut for Visitor {
    /// Desugar the iterator item's body into an underlying unstable `Generator`.
    ///
    /// This takes care of turning `async` iterators into a sync `Generator` body that is
    /// equivalent to the `rustc` desugared `async` code for `async`/`await`.
    fn visit_expr_mut(&mut self, i: &mut syn::Expr) {
        // We traverse all the child nodes first.
        syn::visit_mut::visit_expr_mut(self, i);
        match i {
            // FIXME: consider implementing `for await i in foo {}` syntax here by handling
            // `syn::Expr::ForLoop`.
            // FIXME: attempt to calculate `size_hint` proactively in loops by calling `size_hint`
            // in the expression being iterated *before* building the generator. This can only work
            // in very specific circumstances, so we need to be very clear that we are in one of
            // the valid cases. If we do this, we need to also increment a counter for every
            // `yield` statement outside of loops.
            syn::Expr::Return(syn::ExprReturn { expr, .. }) => {
                // To avoid further type errors down the line, explicitly handle this case and
                // remove it from the resulting item body.
                if let Some(expr) = expr {
                    expr.span()
                        .unwrap()
                        .error("iterator items can't return a non-`()` value")
                        .help("returning in an iterator is only meant for stopping the iterator")
                        .emit();
                }
                *expr = None;
            }
            syn::Expr::Yield(syn::ExprYield {
                expr: Some(expr), ..
            }) if self.is_async => {
                // Turn `yield #expr` in an `async` iterator item into `yield Poll::Ready(#expr)`
                *i = parse_quote!(iterator_item::async_gen_yield!(#expr));
            }
            syn::Expr::Yield(syn::ExprYield { expr: None, .. }) if self.is_async => {
                // Turn `yield;` in an `async` iterator item into `yield Poll::Ready(())`
                *i = parse_quote!(iterator_item::async_gen_yield!(()));
            }
            syn::Expr::Await(syn::ExprAwait { base: expr, .. }) if self.is_async => {
                // Turn `#expr.await` in an `async` iterator item into a `poll(#expr, cxt)` call
                // (with more details, look at the macro for more)
                *i = parse_quote!(iterator_item::async_gen_await!(#expr, __stream_ctx));
            }
            syn::Expr::Try(syn::ExprTry { expr, .. }) => {
                // Turn `#expr?` into one last `yield #expr`
                *i = match self.is_async {
                    true => parse_quote!(iterator_item::async_gen_try!(#expr)),
                    false => parse_quote!(iterator_item::gen_try!(#expr)),
                };
            }
            _ => {}
        }
    }
}

/// Copied from `syn` because it exists but it is private 🤷
fn parse_fn_args(input: ParseStream) -> Result<Punctuated<FnArg, Token![,]>> {
    let mut args = Punctuated::new();
    let mut has_receiver = false;

    while !input.is_empty() {
        let attrs = input.call(Attribute::parse_outer)?;

        let arg = if let Some(dots) = input.parse::<Option<Token![...]>>()? {
            dots.span()
                .unwrap()
                .error("variadic arguments are not allowed in iterator items")
                .emit();
            continue;
        } else {
            let mut arg: FnArg = input.parse()?;
            match &mut arg {
                FnArg::Receiver(receiver) if has_receiver => {
                    return Err(Error::new(
                        receiver.self_token.span,
                        "unexpected second method receiver",
                    ));
                }
                FnArg::Receiver(receiver) if !args.is_empty() => {
                    return Err(Error::new(
                        receiver.self_token.span,
                        "unexpected method receiver",
                    ));
                }
                FnArg::Receiver(receiver) => {
                    has_receiver = true;
                    receiver.attrs = attrs;
                }
                FnArg::Typed(arg) => arg.attrs = attrs,
            }
            arg
        };
        args.push_value(arg);

        if input.is_empty() {
            break;
        }

        let comma: Token![,] = input.parse()?;
        args.push_punct(comma);
    }

    Ok(args)
}
