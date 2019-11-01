#![feature(box_patterns)]

extern crate proc_macro;
extern crate proc_macro2;
use proc_macro::TokenStream;
use proc_macro2::{TokenStream as TokenStream2, Ident, Span};
use quote::quote;
use syn::parse::{Parse, ParseStream};
use syn::punctuated::Punctuated;
use syn::token::Comma;
use syn::{parse_macro_input, parse_quote, parse_str, Block, Expr, ExprLit, ItemFn, Lit, Result, FnArg, PatType, Pat, PatIdent, Stmt};

fn inline_string_literal(e: &Expr) -> (TokenStream2, TokenStream2) {
    let mut bytes = match e {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => s.value().clone().into_bytes(),
        _ => panic!("expected string literal"),
    };
    bytes.push(0u8);
    let len = bytes.len();
    let bytes = bytes;
    let ty = quote!([u8; #len]);
    let array_lit = quote!([#(#bytes),*]);

    (ty, array_lit)
}

struct Args(Punctuated<Expr, Comma>);

impl Parse for Args {
    fn parse(input: ParseStream) -> Result<Args> {
        Ok(Args(Punctuated::parse_terminated(input)?))
    }
}

#[proc_macro]
pub fn probe(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as Args);
    let mut args = input.0.iter();
    let version = args.next().expect("no version");
    let license = args.next().expect("no license");
    let (license_ty, license) = inline_string_literal(&license);
    let mut tokens = quote! {
        #[no_mangle]
        #[link_section = "license"]
        pub static _license: #license_ty = #license;

        #[no_mangle]
        #[link_section = "version"]
        pub static _version: u32 = #version;
    };

    tokens.extend(quote! {
        #[start]
        #[no_mangle]
        pub extern "C" fn _start() -> ! {
            loop {}
        }

        #[lang = "eh_personality"]
        #[no_mangle]
        pub extern "C" fn rust_eh_personality() {}

        #[lang = "eh_unwind_resume"]
        #[no_mangle]
        pub extern "C" fn rust_eh_unwind_resume() {}

        #[lang = "panic_impl"]
        #[no_mangle]
        pub extern "C" fn rust_begin_panic(_: &::core::panic::PanicInfo) -> ! {
            loop {}
        }
    });
    tokens.into()
}

#[proc_macro_attribute]
pub fn map(attrs: TokenStream, item: TokenStream) -> TokenStream {
    let attrs = parse_macro_input!(attrs as Expr);
    let name = match attrs {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => s.value().clone(),
        _ => panic!("expected string literal"),
    };

    let section_name = format!("maps/{}", name);
    let item = TokenStream2::from(item);
    let tokens = quote! {
        #[no_mangle]
        #[link_section = #section_name]
        #item
    };

    tokens.into()
}

fn bpf_helpers(prefix: Option<&str>) -> Block {
    let mut funcs = String::from(include!(concat!(env!("OUT_DIR"), "/gen_helper_funcs.rs")));
    if let Some(prefix) = prefix {
        funcs = funcs.replace(":: redbpf_probes ::", prefix);
    }
    let funcs: Block = parse_str(&funcs).unwrap();

    funcs
}

fn bpf_overrides() -> Block {
    parse_quote! {
        {
            let _bpf_get_current_pid_tgid = bpf_get_current_pid_tgid;
            let bpf_get_current_pid_tgid = || {
                unsafe { _bpf_get_current_pid_tgid() }
            };
            let _bpf_get_current_uid_gid = bpf_get_current_uid_gid;
            let bpf_get_current_uid_gid = || {
                unsafe { _bpf_get_current_uid_gid() }
            };
            let _bpf_get_current_comm = bpf_get_current_comm;
            let bpf_get_current_comm = || {
                let mut comm: [c_char; 16usize] = [0; 16];
                unsafe { _bpf_get_current_comm(&mut comm as *mut _ as *mut c_void, 16u32) };
                comm
            };
        }
    }
}

fn inject_bpf_helpers(item: &mut ItemFn, prefix: Option<&str>) {
    let helpers = bpf_helpers(prefix);
    let overrides = bpf_overrides();
    let mut stmts = helpers.stmts.clone();
    stmts.extend(overrides.stmts);
    stmts.extend(item.block.stmts.clone());
    item.block.stmts = stmts;
}

#[proc_macro_attribute]
pub fn helpers(_attrs: TokenStream, item: TokenStream) -> TokenStream {
    let mut item = parse_macro_input!(item as ItemFn);
    inject_bpf_helpers(&mut item, None);
    let tokens = quote! {
        #item
    };

    tokens.into()
}

#[proc_macro_attribute]
pub fn internal_helpers(_attrs: TokenStream, item: TokenStream) -> TokenStream {
    let mut item = parse_macro_input!(item as ItemFn);
    inject_bpf_helpers(&mut item, Some("crate ::"));
    let tokens = quote! {
        #item
    };

    tokens.into()
}

fn probe_impl(ty: &str, attrs: TokenStream, mut item: ItemFn) -> TokenStream {
    let attrs = parse_macro_input!(attrs as Expr);
    let name = match attrs {
        Expr::Lit(ExprLit {
            lit: Lit::Str(s), ..
        }) => s.value().clone(),
        _ => panic!("expected string literal"),
    };

    let section_name = format!("{}/{}", ty, name);
    inject_bpf_helpers(&mut item, None);
    let tokens = quote! {
        #[no_mangle]
        #[link_section = #section_name]
        #item
    };

    tokens.into()
}

#[proc_macro_attribute]
pub fn kprobe(attrs: TokenStream, item: TokenStream) -> TokenStream {
    let item = parse_macro_input!(item as ItemFn);
    probe_impl("kprobe", attrs, item).into()
}

#[proc_macro_attribute]
pub fn xdp(attrs: TokenStream, item: TokenStream) -> TokenStream {
    let mut item = parse_macro_input!(item as ItemFn);
    let arg = item.sig.inputs.pop().unwrap();
    let ident = match arg.value() {
        FnArg::Typed(PatType { pat: box Pat::Ident(PatIdent { ident, ..}), ..}) => ident,
        _ => panic!("unexpected xdp probe signature")
    };
    let raw_ctx = Ident::new(&format!("_raw_{}", ident), Span::call_site());
    let arg: FnArg = parse_quote! { #raw_ctx: *mut xdp_md };
    item.sig.inputs.push(arg);
    let ctx: Stmt = parse_quote! { let #ident = XdpContext { ctx: #raw_ctx }; };
    item.block.stmts.insert(0, ctx);
    probe_impl("xdp", attrs, item).into()
}
