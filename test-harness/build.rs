use std::{
    env,
    fs,
    path::PathBuf,
};

use quote::quote;
use syn::{
    ImplItem,
    ImplItemFn,
    Item,
    ItemImpl,
    ItemMod,
    Type,
};

fn module<'a>(
    items: &'a [Item],
    name: &str,
) -> &'a ItemMod {
    items
        .iter()
        .find_map(|item| match item {
            Item::Mod(module) if module.ident == name => Some(module),
            _ => None,
        })
        .unwrap_or_else(|| panic!("production module {name} is missing"))
}

fn contents(module: &ItemMod) -> &[Item] {
    &module
        .content
        .as_ref()
        .expect("the harness expects an inline production module")
        .1
}

fn implementation<'a>(
    items: &'a [Item],
    name: &str,
) -> &'a ItemImpl {
    items
        .iter()
        .find_map(|item| match item {
            Item::Impl(implementation) if implementation.trait_.is_none() => {
                match implementation.self_ty.as_ref() {
                    Type::Path(path) if path.path.is_ident(name) => {
                        Some(implementation)
                    }
                    _ => None,
                }
            }
            _ => None,
        })
        .unwrap_or_else(|| panic!("production impl {name} is missing"))
}

fn methods<'a>(
    implementation: &'a ItemImpl,
    names: &[&str],
) -> Vec<&'a ImplItemFn> {
    names
        .iter()
        .map(|name| {
            implementation
                .items
                .iter()
                .find_map(|item| match item {
                    ImplItem::Fn(method) if method.sig.ident == *name => {
                        Some(method)
                    }
                    _ => None,
                })
                .unwrap_or_else(|| {
                    panic!("production method {name} is missing")
                })
        })
        .collect()
}

fn function<'a>(
    items: &'a [Item],
    name: &str,
) -> &'a syn::ItemFn {
    items
        .iter()
        .find_map(|item| match item {
            Item::Fn(function) if function.sig.ident == name => Some(function),
            _ => None,
        })
        .unwrap_or_else(|| panic!("production function {name} is missing"))
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let root = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let source = root.join("../src/main.rs").canonicalize()?;
    println!("cargo:rerun-if-changed={}", source.display());
    let parsed = syn::parse_file(&fs::read_to_string(source)?)?;
    let output = PathBuf::from(env::var_os("OUT_DIR").unwrap());

    let cfg = module(&parsed.items, "cfg");
    fs::write(
        output.join("argument_methods.rs"),
        quote! {
            #[allow(dead_code)]
            #cfg
        }
        .to_string(),
    )?;

    // Import current method ASTs rather than maintaining copies of their logic.
    // Missing/renamed APIs fail the build instead of silently testing old code.
    let x11 = contents(module(&parsed.items, "x11"));
    let app = contents(module(&parsed.items, "app"));
    let context = implementation(app, "Ctx");
    let lifecycle_types = app.iter().filter(|item| {
        matches!(item, Item::Struct(item) if item.ident == "Discovery" || item.ident == "EmbeddedProcess")
    });
    let context_constants = context
        .items
        .iter()
        .filter(|item| matches!(item, ImplItem::Const(_)));
    let focus_host = methods(
        implementation(x11, "X11Host"),
        &["input_focus", "contains_window", "focus_window"],
    );
    let embedded =
        methods(implementation(x11, "EmbeddedWindowMan"), &["focus"]);
    let host = methods(
        implementation(x11, "HostWindowMan"),
        &["hide", "release_focus", "is_focused", "is_parent_of"],
    );
    let focus_context = methods(
        context,
        &[
            "process_x11_events",
            "process_x11_event",
            "embed",
            "update_readiness",
            "toggle",
            "start",
            "reap_hosted",
            "on_child_exit",
            "schedule_discovery",
            "poll_timeout",
            "discovery_timeout",
            "retry_discovery",
            "quit",
        ],
    );
    let focus = quote! {
        #(#lifecycle_types)*
        impl X11Host { #(#focus_host)* }
        impl EmbeddedWindowMan { #(#embedded)* }
        impl HostWindowMan { #(#host)* }
        impl Ctx {
            #(#context_constants)*
            #(#focus_context)*
        }
    };
    fs::write(output.join("focus_methods.rs"), focus.to_string())?;

    let find_argb_visual = function(x11, "find_argb_visual");
    let create_host_window = function(x11, "create_host_window");
    let destroy_host =
        methods(implementation(x11, "HostWindowMan"), &["destroy"]);
    fs::write(
        output.join("visual_methods.rs"),
        quote! {
            #find_argb_visual
            #create_host_window
            impl HostWindowMan { #(#destroy_host)* }
        }
        .to_string(),
    )?;

    let dragons = module(&parsed.items, "dragons");
    let poll_function = function(contents(dragons), "poll");
    let polling = methods(context, &["ekran", "process_x11_events"]);
    let poll = quote! {
        impl Ctx { #(#polling)* }
        mod poll_impl {
            use super::*;
            use libc::pollfd;
            #poll_function
        }
    };
    fs::write(output.join("poll_methods.rs"), poll.to_string())?;

    // Each native test/fixture uses the same checked FFI wrappers as xpop.
    let syscalls = quote! {
        #[allow(dead_code)]
        #dragons
    };
    fs::write(output.join("syscall_methods.rs"), syscalls.to_string())?;
    Ok(())
}
