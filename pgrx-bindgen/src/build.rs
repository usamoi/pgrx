//LICENSE Portions Copyright 2019-2021 ZomboDB, LLC.
//LICENSE
//LICENSE Portions Copyright 2021-2023 Technology Concepts & Design, Inc.
//LICENSE
//LICENSE Portions Copyright 2023-2023 PgCentral Foundation, Inc. <contact@pgcentral.org>
//LICENSE
//LICENSE All rights reserved.
//LICENSE
//LICENSE Use of this source code is governed by the MIT license that can be found in the LICENSE file.
use bindgen::callbacks::{DeriveTrait, EnumVariantValue, ImplementsTrait, MacroParsingBehavior};
use bindgen::NonCopyUnionStyle;
use eyre::WrapErr;
use pgrx_pg_config::{PgConfig, PgMinorVersion, PgVersion};
use quote::{quote, ToTokens};
use std::cell::RefCell;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Read;
use std::path::{Path, PathBuf}; // disambiguate path::Path and syn::Type::Path
use std::rc::Rc;
use syn::{Item, ItemConst};

const BLOCKLISTED_TYPES: [&str; 4] = ["Datum", "NullableDatum", "Oid", "TransactionId"];

// These postgres versions were effectively "yanked" by the community, even tho they still exist
// in the wild.  pgrx will refuse to compile against them
const YANKED_POSTGRES_VERSIONS: &[PgVersion] = &[
    // this set of releases introduced an ABI break in the [`pg_sys::ResultRelInfo`] struct
    // and was replaced by the community on 2024-11-21
    // https://www.postgresql.org/about/news/postgresql-172-166-1510-1415-1318-and-1222-released-2965/
    PgVersion::new(17, PgMinorVersion::Release(1), None),
    PgVersion::new(16, PgMinorVersion::Release(5), None),
    PgVersion::new(15, PgMinorVersion::Release(9), None),
    PgVersion::new(14, PgMinorVersion::Release(14), None),
    PgVersion::new(13, PgMinorVersion::Release(17), None),
];

pub(super) mod clang;

#[derive(Debug)]
struct BindingOverride {
    ignore_macros: HashSet<&'static str>,
    enum_names: InnerMut<EnumMap>,
}

type InnerMut<T> = Rc<RefCell<T>>;
type EnumMap = BTreeMap<String, Vec<(String, EnumVariantValue)>>;

impl BindingOverride {
    fn new_from(enum_names: InnerMut<EnumMap>) -> Self {
        // these cause duplicate definition problems on linux
        // see: https://github.com/rust-lang/rust-bindgen/issues/687
        BindingOverride {
            ignore_macros: HashSet::from_iter([
                "FP_INFINITE",
                "FP_NAN",
                "FP_NORMAL",
                "FP_SUBNORMAL",
                "FP_ZERO",
                "IPPORT_RESERVED",
                // These are just annoying due to clippy
                "M_E",
                "M_LOG2E",
                "M_LOG10E",
                "M_LN2",
                "M_LN10",
                "M_PI",
                "M_PI_2",
                "M_PI_4",
                "M_1_PI",
                "M_2_PI",
                "M_SQRT2",
                "M_SQRT1_2",
                "M_2_SQRTPI",
            ]),
            enum_names,
        }
    }
}

impl bindgen::callbacks::ParseCallbacks for BindingOverride {
    fn will_parse_macro(&self, name: &str) -> MacroParsingBehavior {
        if self.ignore_macros.contains(name) {
            bindgen::callbacks::MacroParsingBehavior::Ignore
        } else {
            bindgen::callbacks::MacroParsingBehavior::Default
        }
    }

    fn blocklisted_type_implements_trait(
        &self,
        name: &str,
        derive_trait: DeriveTrait,
    ) -> Option<ImplementsTrait> {
        if !BLOCKLISTED_TYPES.contains(&name) {
            return None;
        }

        let implements_trait = match derive_trait {
            DeriveTrait::Copy => ImplementsTrait::Yes,
            DeriveTrait::Debug => ImplementsTrait::Yes,
            _ => ImplementsTrait::No,
        };
        Some(implements_trait)
    }

    // FIXME: alter types on some int macros to the actually-used types so we can stop as-casting them
    fn int_macro(&self, _name: &str, _value: i64) -> Option<bindgen::callbacks::IntKind> {
        None
    }

    // FIXME: implement a... C compiler?
    fn func_macro(&self, _name: &str, _value: &[&[u8]]) {}

    /// Intentionally doesn't do anything, just updates internal state.
    fn enum_variant_behavior(
        &self,
        enum_name: Option<&str>,
        variant_name: &str,
        variant_value: bindgen::callbacks::EnumVariantValue,
    ) -> Option<bindgen::callbacks::EnumVariantCustomBehavior> {
        enum_name.inspect(|name| match name.strip_prefix("enum").unwrap_or(name).trim() {
            // specifically overridden enum
            "NodeTag" => (),
            name if name.contains("unnamed at") || name.contains("anonymous at") => (),
            // to prevent problems with BuiltinOid
            _ if variant_name.contains("OID") => (),
            name => self
                .enum_names
                .borrow_mut()
                .entry(name.to_string())
                .or_default()
                .push((variant_name.to_string(), variant_value)),
        });
        None
    }

    // FIXME: hide nodetag fields and default them to appropriate values
    fn field_visibility(
        &self,
        _info: bindgen::callbacks::FieldInfo<'_>,
    ) -> Option<bindgen::FieldVisibilityKind> {
        None
    }
}

/// Given a token stream representing a file, apply a series of transformations to munge
/// the bindgen generated code with some postgres specific enhancements
fn rewrite_items(
    mut file: syn::File,
    oids: &BTreeMap<syn::Ident, Box<syn::Expr>>,
) -> eyre::Result<proc_macro2::TokenStream> {
    fix_linkage(&mut file);
    rewrite_c_abi_to_c_unwind(&mut file);
    let items_vec = rewrite_oid_consts(&file.items, oids);
    let mut items = apply_pg_guard(&items_vec)?;
    let pgnode_impls = impl_pg_node(&items_vec)?;

    // append the pgnodes to the set of items
    items.extend(pgnode_impls);

    Ok(items)
}

/// Find all the constants that represent Postgres type OID values.
///
/// These are constants of type `u32` whose name ends in the string "OID"
fn extract_oids(code: &syn::File) -> BTreeMap<syn::Ident, Box<syn::Expr>> {
    let mut oids = BTreeMap::new(); // we would like to have a nice sorted set
    for item in &code.items {
        let Item::Const(ItemConst { ident, ty, expr, .. }) = item else { continue };
        // Retype as strings for easy comparison
        let name = ident.to_string();
        let ty_str = ty.to_token_stream().to_string();

        // This heuristic identifies "OIDs"
        // We're going to warp the const declarations to be our newtype Oid
        if ty_str == "u32" && is_builtin_oid(&name) {
            oids.insert(ident.clone(), expr.clone());
        }
    }
    oids
}

fn is_builtin_oid(name: &str) -> bool {
    name.ends_with("OID") && name != "HEAP_HASOID"
        || name.ends_with("RelationId")
        || name == "TemplateDbOid"
}

fn rewrite_oid_consts(
    items: &[syn::Item],
    oids: &BTreeMap<syn::Ident, Box<syn::Expr>>,
) -> Vec<syn::Item> {
    items
        .iter()
        .map(|item| match item {
            Item::Const(ItemConst { ident, ty, expr, .. })
                if ty.to_token_stream().to_string() == "u32" && oids.get(ident) == Some(expr) =>
            {
                syn::parse2(quote! { pub const #ident : Oid = Oid(#expr); }).unwrap()
            }
            item => item.clone(),
        })
        .collect()
}

fn format_builtin_oid_impl(oids: BTreeMap<syn::Ident, Box<syn::Expr>>) -> proc_macro2::TokenStream {
    let enum_variants: proc_macro2::TokenStream;
    let from_impl: proc_macro2::TokenStream;
    (enum_variants, from_impl) = oids
        .iter()
        .map(|(ident, expr)| {
            (quote! { #ident = #expr, }, quote! { #expr => Ok(BuiltinOid::#ident), })
        })
        .unzip();

    quote! {
        use crate::{NotBuiltinOid};

        #[derive(Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd, Debug)]
        pub enum BuiltinOid {
            #enum_variants
        }

        impl BuiltinOid {
            pub const fn from_u32(uint: u32) -> Result<BuiltinOid, NotBuiltinOid> {
                match uint {
                    0 => Err(NotBuiltinOid::Invalid),
                    #from_impl
                    _ => Err(NotBuiltinOid::Ambiguous),
                }
            }
        }
    }
}

/// Implement our `PgNode` marker trait for `pg_sys::Node` and its "subclasses"
fn impl_pg_node(items: &[syn::Item]) -> eyre::Result<proc_macro2::TokenStream> {
    let mut pgnode_impls = proc_macro2::TokenStream::new();

    // we scope must of the computation so we can borrow `items` and then
    // extend it at the very end.
    let struct_graph: StructGraph = StructGraph::from(items);

    // collect all the structs with `NodeTag` as their first member,
    // these will serve as roots in our forest of `Node`s
    let mut root_node_structs = Vec::new();
    for descriptor in struct_graph.descriptors.iter() {
        // grab the first field, if any
        let first_field = match &descriptor.struct_.fields {
            syn::Fields::Named(fields) => {
                if let Some(first_field) = fields.named.first() {
                    first_field
                } else {
                    continue;
                }
            }
            syn::Fields::Unnamed(fields) => {
                if let Some(first_field) = fields.unnamed.first() {
                    first_field
                } else {
                    continue;
                }
            }
            _ => continue,
        };

        // grab the type name of the first field
        let ty_name = if let syn::Type::Path(p) = &first_field.ty {
            if let Some(last_segment) = p.path.segments.last() {
                last_segment.ident.to_string()
            } else {
                continue;
            }
        } else {
            continue;
        };

        if ty_name == "NodeTag" {
            root_node_structs.push(descriptor);
        }
    }

    // the set of types which subclass `Node` according to postgres' object system
    let mut node_set = BTreeSet::new();
    // fill in any children of the roots with a recursive DFS
    // (we are not operating on user input, so it is ok to just
    //  use direct recursion rather than an explicit stack).
    for root in root_node_structs.into_iter() {
        dfs_find_nodes(root, &struct_graph, &mut node_set);
    }

    // now we can finally iterate the Nodes and emit out Display impl
    for node_struct in node_set.into_iter() {
        let struct_name = &node_struct.struct_.ident;

        // impl the PgNode trait for all nodes
        pgnode_impls.extend(quote! {
            impl pg_sys::seal::Sealed for #struct_name {}
            impl pg_sys::PgNode for #struct_name {}
        });

        // impl Rust's Display trait for all nodes
        pgnode_impls.extend(quote! {
            impl ::core::fmt::Display for #struct_name {
                fn fmt(&self, f: &mut ::core::fmt::Formatter<'_>) -> ::core::fmt::Result {
                    self.display_node().fmt(f)
                }
            }
        });
    }

    Ok(pgnode_impls)
}

/// Given a root node, dfs_find_nodes adds all its children nodes to `node_set`.
fn dfs_find_nodes<'graph>(
    node: &'graph StructDescriptor<'graph>,
    graph: &'graph StructGraph<'graph>,
    node_set: &mut BTreeSet<StructDescriptor<'graph>>,
) {
    node_set.insert(node.clone());

    for child in node.children(graph) {
        if node_set.contains(child) {
            continue;
        }
        dfs_find_nodes(child, graph, node_set);
    }
}

/// A graph describing the inheritance relationships between different nodes
/// according to postgres' object system.
///
/// NOTE: the borrowed lifetime on a StructGraph should also ensure that the offsets
///       it stores into the underlying items struct are always correct.
#[derive(Clone, Debug)]
struct StructGraph<'a> {
    #[allow(dead_code)]
    /// A table mapping struct names to their offset in the descriptor table
    name_tab: HashMap<String, usize>,
    #[allow(dead_code)]
    /// A table mapping offsets into the underlying items table to offsets in the descriptor table
    item_offset_tab: Vec<Option<usize>>,
    /// A table of struct descriptors
    descriptors: Vec<StructDescriptor<'a>>,
}

impl<'a> From<&'a [syn::Item]> for StructGraph<'a> {
    fn from(items: &'a [syn::Item]) -> StructGraph<'a> {
        let mut descriptors = Vec::new();

        // a table mapping struct names to their offset in `descriptors`
        let mut name_tab: HashMap<String, usize> = HashMap::new();
        let mut item_offset_tab: Vec<Option<usize>> = vec![None; items.len()];
        for (i, item) in items.iter().enumerate() {
            if let &syn::Item::Struct(struct_) = &item {
                let next_offset = descriptors.len();
                descriptors.push(StructDescriptor {
                    struct_,
                    items_offset: i,
                    parent: None,
                    children: Vec::new(),
                });
                name_tab.insert(struct_.ident.to_string(), next_offset);
                item_offset_tab[i] = Some(next_offset);
            }
        }

        for item in items.iter() {
            // grab the first field if it is struct
            let (id, first_field) = match &item {
                syn::Item::Struct(syn::ItemStruct {
                    ident: id,
                    fields: syn::Fields::Named(fields),
                    ..
                }) => {
                    if let Some(first_field) = fields.named.first() {
                        (id.to_string(), first_field)
                    } else {
                        continue;
                    }
                }
                &syn::Item::Struct(syn::ItemStruct {
                    ident: id,
                    fields: syn::Fields::Unnamed(fields),
                    ..
                }) => {
                    if let Some(first_field) = fields.unnamed.first() {
                        (id.to_string(), first_field)
                    } else {
                        continue;
                    }
                }
                _ => continue,
            };

            if let syn::Type::Path(p) = &first_field.ty {
                // We should be guaranteed that just extracting the last path
                // segment is ok because these structs are all from the same module.
                // (also, they are all generated from C code, so collisions should be
                //  impossible anyway thanks to C's single shared namespace).
                if let Some(last_segment) = p.path.segments.last() {
                    if let Some(parent_offset) = name_tab.get(&last_segment.ident.to_string()) {
                        // establish the 2-way link
                        let child_offset = name_tab[&id];
                        descriptors[child_offset].parent = Some(*parent_offset);
                        descriptors[*parent_offset].children.push(child_offset);
                    }
                }
            }
        }

        StructGraph { name_tab, item_offset_tab, descriptors }
    }
}

impl<'a> StructDescriptor<'a> {
    /// children returns an iterator over the children of this node in the graph
    fn children(&'a self, graph: &'a StructGraph) -> StructDescriptorChildren<'a> {
        StructDescriptorChildren { offset: 0, descriptor: self, graph }
    }
}

/// An iterator over a StructDescriptor's children
struct StructDescriptorChildren<'a> {
    offset: usize,
    descriptor: &'a StructDescriptor<'a>,
    graph: &'a StructGraph<'a>,
}

impl<'a> std::iter::Iterator for StructDescriptorChildren<'a> {
    type Item = &'a StructDescriptor<'a>;
    fn next(&mut self) -> Option<&'a StructDescriptor<'a>> {
        if self.offset >= self.descriptor.children.len() {
            None
        } else {
            let ret = Some(&self.graph.descriptors[self.descriptor.children[self.offset]]);
            self.offset += 1;
            ret
        }
    }
}

/// A node a StructGraph
#[derive(Clone, Debug, Hash, Eq, PartialEq)]
struct StructDescriptor<'a> {
    /// A reference to the underlying struct syntax node
    struct_: &'a syn::ItemStruct,
    /// An offset into the items slice that was used to construct the struct graph that
    /// this StructDescriptor is a part of
    items_offset: usize,
    /// The offset of the "parent" (first member) struct (if any).
    parent: Option<usize>,
    /// The offsets of the "children" structs (if any).
    children: Vec<usize>,
}

impl PartialOrd for StructDescriptor<'_> {
    #[inline]
    fn partial_cmp(&self, other: &StructDescriptor) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for StructDescriptor<'_> {
    #[inline]
    fn cmp(&self, other: &StructDescriptor) -> Ordering {
        self.struct_.ident.cmp(&other.struct_.ident)
    }
}

/// Given a specific postgres version, `run_bindgen` generates bindings for the given
/// postgres version and returns them.
pub fn generate_bindings(
    target_os: &str,
    target_env: &str,
    pg_config: &PgConfig,
    bindgen_no_detect_includes: bool,
) -> eyre::Result<(String, String, String)> {
    let version = pg_config.get_version()?;
    if YANKED_POSTGRES_VERSIONS.contains(&version) {
        panic!(
            "Postgres v{}{} is incompatible with \
                other versions in this major series and is not supported by pgrx.  Please upgrade \
                to the latest version in the v{} series.",
            version.major, version.minor, version.major
        );
    }
    let major_version = pg_config.major_version()?;
    eprintln!("Generating bindings for pg{major_version}");
    let contents = match major_version {
        13 => include_str!("../assets/pg13.h"),
        14 => include_str!("../assets/pg14.h"),
        15 => include_str!("../assets/pg15.h"),
        16 => include_str!("../assets/pg16.h"),
        17 => include_str!("../assets/pg17.h"),
        _ => eyre::bail!("unsupported postgres version"),
    };
    let configure = pg_config.configure()?;
    let preferred_clang: Option<&std::path::Path> = configure.get("CLANG").map(|s| s.as_ref());
    eprintln!("pg_config --configure CLANG = {preferred_clang:?}");
    let pg_target_includes = pg_target_includes(target_env, pg_config)?;
    eprintln!("pg_target_includes = {pg_target_includes:?}");
    let (autodetect, includes) = if !bindgen_no_detect_includes {
        clang::detect_include_paths_for(preferred_clang)
    } else {
        (false, vec![])
    };
    let mut binder = bindgen::Builder::default();
    binder = add_blocklists(binder);
    binder = add_allowlists(binder, pg_target_includes.iter().map(|x| x.as_str()));
    binder = add_derives(binder);
    if !autodetect {
        let builtin_includes = includes.iter().filter_map(|p| Some(format!("-I{}", p.to_str()?)));
        binder = binder.clang_args(builtin_includes);
    };
    let enum_names = Rc::new(RefCell::new(BTreeMap::new()));
    let overrides = BindingOverride::new_from(Rc::clone(&enum_names));
    let temppath = tempfile::NamedTempFile::with_suffix(".c")?.into_temp_path();
    let bindings = binder
        .header_contents("pgrx.h", contents)
        .clang_args(extra_bindgen_clang_args(target_os, pg_config)?)
        .clang_args(pg_target_includes.iter().map(|x| format!("-I{x}")))
        .detect_include_paths(autodetect)
        .parse_callbacks(Box::new(overrides))
        .default_enum_style(bindgen::EnumVariation::ModuleConsts)
        // The NodeTag enum is closed: additions break existing values in the set, so it is not extensible
        .rustified_non_exhaustive_enum("NodeTag")
        .size_t_is_usize(true)
        .merge_extern_blocks(true)
        .wrap_unsafe_ops(true)
        .use_core()
        .generate_cstr(true)
        .disable_nested_struct_naming()
        .formatter(bindgen::Formatter::None)
        .layout_tests(false)
        .default_non_copy_union_style(NonCopyUnionStyle::ManuallyDrop)
        .wrap_static_fns(true)
        .wrap_static_fns_path(temppath.with_extension(""))
        .wrap_static_fns_suffix("__pgrx_cshim")
        .generate()
        .wrap_err_with(|| format!("Unable to generate bindings for pg{major_version}"))?;
    let mut binding_str = bindings.to_string();
    drop(bindings); // So the Rc::into_inner can unwrap

    // FIXME: do this for the Node graph instead of reparsing?
    let enum_names: EnumMap = Rc::into_inner(enum_names).unwrap().into_inner();
    binding_str.extend(enum_names.into_iter().flat_map(|(name, variants)| {
        const MIN_I32: i64 = i32::MIN as _;
        const MAX_I32: i64 = i32::MAX as _;
        const MAX_U32: u64 = u32::MAX as _;
        variants.into_iter().map(move |(variant, value)| {
            let (ty, value) = match value {
                EnumVariantValue::Boolean(b) => ("bool", b.to_string()),
                EnumVariantValue::Signed(v @ MIN_I32..=MAX_I32) => ("i32", v.to_string()),
                EnumVariantValue::Signed(v) => ("i64", v.to_string()),
                EnumVariantValue::Unsigned(v @ 0..=MAX_U32) => ("u32", v.to_string()),
                EnumVariantValue::Unsigned(v) => ("u64", v.to_string()),
            };
            format!(
                r#"
#[deprecated(since = "0.12.0", note = "you want pg_sys::{module}::{variant}")]
pub const {module}_{variant}: {ty} = {value};"#,
                module = &*name, // imprecise closure capture
            )
        })
    }));
    binding_str.push_str(include_str!("../assets/cshim.rs"));

    let bindgen_output =
        syn::parse_file(&binding_str).wrap_err_with(|| "failed to parse generated bindings")?;

    let oids = extract_oids(&bindgen_output);
    let rewritten_items = rewrite_items(bindgen_output, &oids)
        .wrap_err_with(|| format!("failed to rewrite items for pg{major_version}"))?;
    let binding = {
        let mut contents = quote! {
            use crate as pg_sys;
            use crate::{Datum, MultiXactId, Oid, PgNode, TransactionId};
        };
        contents.extend(rewritten_items);
        contents
    };
    let binding_oids = format_builtin_oid_impl(oids);

    let header = "/* Automatically generated by bindgen. Do not hand-edit. */";
    let binding = format!("{header}\n{}", prettyplease::unparse(&syn::parse2(binding)?));
    let binding_oids = format!("{header}\n{}", prettyplease::unparse(&syn::parse2(binding_oids)?));
    let binding_cshim = format!(
        "{header}\n{contents}\n{}\n{}",
        std::fs::read_to_string(temppath)?,
        include_str!("../assets/cshim.c")
    );

    Ok((binding, binding_oids, binding_cshim))
}

fn add_blocklists(bind: bindgen::Builder) -> bindgen::Builder {
    bind.blocklist_type("Datum") // manually wrapping datum for correctness
        .blocklist_type("Oid") // "Oid" is not just any u32
        .blocklist_type("TransactionId") // "TransactionId" is not just any u32
        .blocklist_type("MultiXactId") // it's an alias of "TransactionId"
        .blocklist_var("CONFIGURE_ARGS") // configuration during build is hopefully irrelevant
        .blocklist_var("_*(?:HAVE|have)_.*") // header tracking metadata
        .blocklist_var("_[A-Z_]+_H") // more header metadata
        // It's used by explict `extern "C-unwind"`
        .blocklist_function("pg_re_throw")
        .blocklist_function("err(start|code|msg|detail|context_msg|hint|finish)")
        // These functions are already ported in Rust
        .blocklist_function("heap_getattr")
        .blocklist_function("BufferGetBlock")
        .blocklist_function("BufferGetPage")
        .blocklist_function("BufferIsLocal")
        .blocklist_function("GetMemoryChunkContext")
        .blocklist_function("GETSTRUCT")
        .blocklist_function("MAXALIGN")
        .blocklist_function("MemoryContextIsValid")
        .blocklist_function("MemoryContextSwitchTo")
        .blocklist_function("TYPEALIGN")
        .blocklist_function("TransactionIdIsNormal")
        .blocklist_function("expression_tree_walker")
        .blocklist_function("get_pg_major_minor_version_string")
        .blocklist_function("get_pg_major_version_num")
        .blocklist_function("get_pg_major_version_string")
        .blocklist_function("get_pg_version_string")
        .blocklist_function("heap_tuple_get_struct")
        .blocklist_function("planstate_tree_walker")
        .blocklist_function("query_or_expression_tree_walker")
        .blocklist_function("query_tree_walker")
        .blocklist_function("range_table_entry_walker")
        .blocklist_function("range_table_walker")
        .blocklist_function("raw_expression_tree_walker")
        .blocklist_function("type_is_array")
        .blocklist_function("varsize_any")
        // it's defined twice on Windows, so use PGERROR instead
        .blocklist_item("ERROR")
        // it causes strange linker errors for PostgreSQL 14 on Windows
        .blocklist_function("IsQueryIdEnabled")
}

fn add_allowlists<'a>(
    mut bind: bindgen::Builder,
    pg_target_includes: impl Iterator<Item = &'a str>,
) -> bindgen::Builder {
    for pg_target_include in pg_target_includes {
        bind = bind.allowlist_file(format!("{}.*", regex::escape(pg_target_include)))
    }
    bind.allowlist_item("PGERROR").allowlist_item("SIG.*")
}

fn add_derives(bind: bindgen::Builder) -> bindgen::Builder {
    bind.derive_debug(true)
        .derive_copy(true)
        .derive_default(true)
        .derive_eq(false)
        .derive_partialeq(false)
        .derive_hash(false)
        .derive_ord(false)
        .derive_partialord(false)
}

fn find_include(path: PathBuf) -> eyre::Result<String> {
    let path = std::fs::canonicalize(&path)
        .wrap_err(format!("cannot find {path:?} for C header files"))?
        .join("") // returning a `/`-ending path
        .display()
        .to_string();
    if let Some(path) = path.strip_prefix("\\\\?\\") {
        Ok(path.to_string())
    } else {
        Ok(path)
    }
}

fn pg_target_includes(target_env: &str, pg_config: &PgConfig) -> eyre::Result<Vec<String>> {
    let mut result = vec![find_include(pg_config.includedir_server()?)?];
    if target_env == "msvc" {
        result.push(find_include(pg_config.pkgincludedir()?)?);
        result.push(find_include(pg_config.includedir_server_port_win32()?)?);
        result.push(find_include(pg_config.includedir_server_port_win32_msvc()?)?);
    }
    Ok(result)
}

pub fn build_cshim(
    out_dir: impl AsRef<Path>,
    target_os: &str,
    target_env: &str,
    pg_config: &PgConfig,
) -> eyre::Result<()> {
    let mut build = cc::Build::new();
    let compiler = build.get_compiler();
    if compiler.is_like_gnu() || compiler.is_like_clang() {
        build.flag("-ffunction-sections");
        build.flag("-fdata-sections");
    }
    if compiler.is_like_msvc() {
        build.flag("/Gy");
        build.flag("/Gw");
    }
    for pg_target_include in pg_target_includes(target_env, pg_config)?.iter() {
        build.flag(format!("-I{pg_target_include}"));
    }
    for flag in extra_bindgen_clang_args(target_os, pg_config)? {
        build.flag(&flag);
    }
    build.file(out_dir.as_ref().join(format!("binding_cshim.c")));
    build.opt_level(3);
    build.compile("pgrx-cshim");
    Ok(())
}

fn extra_bindgen_clang_args(target_os: &str, pg_config: &PgConfig) -> eyre::Result<Vec<String>> {
    let mut out = vec![];
    let flags = shlex::split(&pg_config.cppflags()?.to_string_lossy()).unwrap_or_default();
    if target_os != "windows" {
        // Just give clang the full flag set, since presumably that's what we're
        // getting when we build the C shim anyway.
        // Skip it on Windows, since clang is used to generate cshim but MSVC is
        // used to compile PostgreSQL.
        out.extend(flags.iter().cloned());
    }
    if target_os != "macos" {
        // Find the `-isysroot` flags so we can warn about them, so something
        // reasonable shows up if/when the build fails.
        //
        // TODO(thom): Could probably fix some brew/xcode issues here in the
        // Find the `-isysroot` flags so we can warn about them, so something
        // reasonable shows up if/when the build fails.
        //
        // - Handle homebrew packages initially linked against as keg-only, but
        //   which have had their version bumped.
        for pair in flags.windows(2) {
            if pair[0] == "-isysroot" {
                if !std::path::Path::new(&pair[1]).exists() {
                    // The SDK path doesn't exist. Emit a warning, which they'll
                    // see if the build ends up failing (it may not fail in all
                    // cases, so we don't panic here).
                    //
                    // There's a bunch of smarter things we can try here, but
                    // most of them either break things that currently work, or
                    // are very difficult to get right. If you try to fix this,
                    // be sure to consider cases like:
                    //
                    // - User may have CommandLineTools and not Xcode, vice
                    //   versa, or both installed.
                    // - User may using a newer SDK than their OS, or vice
                    //   versa.
                    // - User may be using a newer SDK than their XCode (updated
                    //   Command line tools, not OS), or vice versa.
                    // - And so on.
                    //
                    // These are all actually fairly common. Note that the code
                    // as-is is *not* broken in these cases (except on OS/SDK
                    // updates), so care should be taken to avoid changing that
                    // if possible.
                    //
                    // The logic we'd like ideally is for `cargo pgrx init` to
                    // choose a good SDK in the first place, and force postgres
                    // to use it. Then, the logic in this build script would
                    // Just Work without changes (since we are using its
                    // sysroot verbatim).
                    //
                    // The value of "Good" here is tricky, but the logic should
                    // probably:
                    //
                    // - prefer SDKs from the CLI tools to ones from XCode
                    //   (since they're guaranteed compatible with the user's OS
                    //   version)
                    //
                    // - prefer SDKs that specify only the major SDK version
                    //   (e.g. MacOSX12.sdk and not MacOSX12.4.sdk or
                    //   MacOSX.sdk), to avoid breaking too frequently (if we
                    //   have a minor version) or being totally unable to detect
                    //   what version of the SDK was used to build postgres (if
                    //   we have neither).
                    //
                    // - Avoid choosing an SDK newer than the user's OS version,
                    //   since postgres fails to detect that they are missing if
                    //   you do.
                    //
                    // This is surprisingly hard to implement, as the
                    // information is scattered across a dozen ini files.
                    // Presumably Apple assumes you'll use
                    // `MACOSX_DEPLOYMENT_TARGET`, rather than basing it off the
                    // SDK version, but it's not an option for postgres.
                    let major_version = pg_config.major_version()?;
                    println!(
                        "cargo:warning=postgres v{major_version} was compiled against an \
                         SDK Root which does not seem to exist on this machine ({}). You may \
                         need to re-run `cargo pgrx init` and/or update your command line tools.",
                        pair[1],
                    );
                };
                // Either way, we stop here.
                break;
            }
        }
    }
    Ok(out)
}

fn apply_pg_guard(items: &Vec<syn::Item>) -> eyre::Result<proc_macro2::TokenStream> {
    let mut out = proc_macro2::TokenStream::new();
    for item in items {
        match item {
            Item::ForeignMod(block) => {
                out.extend(quote! {
                    #[pgrx_macros::pg_guard]
                    #block
                });
            }
            _ => {
                out.extend(item.into_token_stream());
            }
        }
    }

    Ok(out)
}

fn fix_linkage(file: &mut syn::File) {
    use syn::visit_mut::VisitMut;
    use syn::{Expr, ExprLit, Lit};
    pub struct Visitor {}
    impl VisitMut for Visitor {
        fn visit_foreign_item_fn_mut(&mut self, func: &mut syn::ForeignItemFn) {
            let link_with_cshim = func.attrs.iter().any(|attr| match &attr.meta {
                syn::Meta::NameValue(kv) if kv.path.is_ident("link_name") => {
                    if let Expr::Lit(ExprLit { lit: Lit::Str(value), .. }) = &kv.value {
                        value.value().ends_with("__pgrx_cshim")
                    } else {
                        false
                    }
                }
                _ => false,
            });
            if link_with_cshim {
                func.attrs.insert(0, syn::parse_quote! { #[cfg(feature = "cshim")] });
            } else {
                func.attrs.insert(
                    0,
                    syn::parse_quote! { #[cfg_attr(target_os = "windows", link(name = "postgres"))] },
                );
            }
        }
        fn visit_foreign_item_static_mut(&mut self, variable: &mut syn::ForeignItemStatic) {
            variable.attrs.insert(
                0,
                syn::parse_quote! { #[cfg_attr(target_os = "windows", link(name = "postgres"))] },
            );
        }
    }
    Visitor {}.visit_file_mut(file);
}

fn rewrite_c_abi_to_c_unwind(file: &mut syn::File) {
    use proc_macro2::Span;
    use syn::visit_mut::VisitMut;
    use syn::LitStr;
    pub struct Visitor {}
    impl VisitMut for Visitor {
        fn visit_abi_mut(&mut self, abi: &mut syn::Abi) {
            if let Some(name) = &mut abi.name {
                if name.value() == "C" {
                    *name = LitStr::new("C-unwind", Span::call_site());
                }
            }
        }
    }
    Visitor {}.visit_file_mut(file);
}

pub fn unzip(path: impl AsRef<Path>) -> eyre::Result<(String, String, String)> {
    let mut ar = zip::ZipArchive::new(std::fs::File::open(path)?)?;
    let mut binding = String::new();
    ar.by_name("binding.rs")?.read_to_string(&mut binding)?;
    let mut binding_oids = String::new();
    ar.by_name("binding_oids.rs")?.read_to_string(&mut binding_oids)?;
    let mut binding_cshim = String::new();
    ar.by_name("binding_cshim.c")?.read_to_string(&mut binding_cshim)?;
    Ok((binding, binding_oids, binding_cshim))
}
