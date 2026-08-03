use proc_macro::TokenStream;
use quote::quote;
use syn::{FnArg, ItemFn, parse_macro_input};

#[proc_macro_attribute]
pub fn firecracker_test(_attribute: TokenStream, item: TokenStream) -> TokenStream {
    let function = parse_macro_input!(item as ItemFn);
    let attributes = function.attrs;
    let visibility = function.vis;
    let mut signature = function.sig;
    let body = function.block;
    let function_name = signature.ident.to_string();
    let vm_id = match signature.inputs.len() {
        0 => quote!(let _firecracker_test_vm_id =
            crate::support::next_firecracker_vm_id();),
        1 => {
            let Some(FnArg::Typed(argument)) = signature.inputs.first() else {
                return syn::Error::new_spanned(
                    &signature.inputs,
                    "firecracker tests cannot have a receiver",
                )
                .into_compile_error()
                .into();
            };
            let pattern = &argument.pat;
            quote!(let #pattern = crate::support::next_firecracker_vm_id();)
        }
        _ => {
            return syn::Error::new_spanned(
                &signature.inputs,
                "firecracker tests accept at most one VmId argument",
            )
            .into_compile_error()
            .into();
        }
    };
    signature.inputs.clear();
    let test_attribute = if signature.asyncness.is_some() {
        quote!(#[tokio::test(flavor = "multi_thread")])
    } else {
        quote!(#[test])
    };

    quote! {
        #(#attributes)*
        #test_attribute
        #[tracing_test::traced_test]
        #[ignore = "requires Linux, KVM, Firecracker, and VM artifacts"]
        #[allow(clippy::await_holding_lock)]
        #visibility #signature {
            let _firecracker_test_guard = crate::support::lock_firecracker_tests();
            #vm_id
            let firecracker = std::env::var_os("FIRECRACKER")
                .is_some_and(|path| std::path::Path::new(&path).is_file());
            let image_root = std::env::var_os("IMAGE_ROOT")
                .is_some_and(|path| std::path::Path::new(&path).is_dir());
            let prerequisites = cfg!(target_os = "linux")
                && std::path::Path::new("/dev/kvm").exists()
                && firecracker
                && image_root;

            if !prerequisites {
                let run_on_lima = std::env::var_os("RUN_ON_LIMA").is_some();
                let already_in_lima =
                    std::env::var_os("BARBIROLLI_LIMA_GUEST").is_some();
                if run_on_lima && !already_in_lima {
                    let instance = std::env::var("LIMA_INSTANCE")
                        .unwrap_or_else(|_| "mvm".to_owned());
                    let current_dir = std::env::current_dir()
                        .expect("the current directory is required to run the test in Lima");
                    let status = std::process::Command::new("limactl")
                        .args(["shell", "--start", "--preserve-env", "--workdir"])
                        .arg(current_dir)
                        .arg(instance)
                        .args([
                            "env",
                            "BARBIROLLI_LIMA_GUEST=1",
                            "CARGO_TARGET_DIR=/var/tmp/barbirolli-target",
                            "cargo",
                            "test",
                            "-p",
                            "barbirolli",
                            "--lib",
                            #function_name,
                            "--",
                            "--ignored",
                            "--nocapture",
                        ])
                        .status()
                        .expect("failed to launch the privileged test through Lima");
                    assert!(status.success(), "the privileged Lima test failed");
                    return;
                }
                panic!(
                    "install KVM, Firecracker, and test artifacts or enable RUN_ON_LIMA"
                );
            }

            #body
        }
    }
    .into()
}
