//! `#[derive(UnionSchema)]` — emit the trait + per-type ToSql/FromSql glue
//! for the Turso backend's UNION support (`diesel::turso::union`).
//!
//! Variant shape → wire shape:
//!
//! * `Variant(T)` (single tuple field) → **scalar** UNION variant. Outer
//!   record column is the scalar value itself.
//! * `Variant { a: Ta, b: Tb, … }` (named fields) → **struct** UNION
//!   variant. Outer record column is a BLOB containing the inner struct's
//!   own SQLite record.
//! * `#[union(boxed)] Variant(Box<T>)` → **boxed struct** variant. Same
//!   wire format as a struct variant, but the enum holds one pointer
//!   (`Box<T>`) instead of inlining all the fields — keeps big variants
//!   from blowing up the enum's stack size. `T` must derive
//!   [`UnionStructPayload`].
//! * `Variant(T1, T2, …)` (tuple with >1 field) → error; use named fields
//!   for multi-field variants (keeps the Rust↔SQL mapping unambiguous).
//! * `Variant` (unit) → error; pick a scalar placeholder if you need a
//!   "none of the above" case.
//!
//! # Names in the DDL are not derived from Rust idents by default
//!
//! They are derived *by* default and that default is frequently wrong, so
//! the attributes exist and the golden tests exist to make a wrong one
//! visible. `snake_case(VariantIdent)` turns `WhatsAppContact` into
//! `whats_app_contact` where every migration in this repo says
//! `whatsapp_contact`, and the struct type for a variant defaults to
//! `<tag>_t` where the migrations use semantic names (`telegram_mid`,
//! `signal_contact_data_v2`) and share one type across several variants
//! (`whatsapp_dialog_id` covers contact, group and newsletter). None of
//! that is a defect in the migrations: a struct type is a schema object
//! with a life of its own, and `<tag>_t` cannot express a shared one.
//!
//! So:
//!
//! * `#[union(name = "…")]` on the enum — the UNION type's name.
//!   Defaults to `snake_case(EnumIdent)`.
//! * `#[union(tag = "…")]` on a variant — its tag name in the DDL and in
//!   `union_extract(col, '…')`. Defaults to `snake_case(VariantIdent)`.
//!   The *wire* tag is the declaration index either way; this is the name.
//! * `#[union(struct_type = "…")]` on a struct or boxed variant — the name
//!   of its `CREATE TYPE … AS STRUCT(…)`. Defaults to `<tag>_t`. Two
//!   variants may name the same type, in which case one statement is
//!   emitted (they must then have identical field lists, which the golden
//!   test checks).
//! * `#[union(sql_type = …)]` on a field — the SQL type it is stored as,
//!   overriding [`TursoFieldType`]'s default for its Rust type. The escape
//!   hatch for a Rust type whose storage is a per-field decision rather
//!   than a property of the type, e.g. a `Vec<SharedString>` stored as a
//!   JSON array in TEXT.
//!
//! # `#[derive(UnionStructPayload)]`
//!
//! Implement the `UnionStructPayload` trait on a plain struct with named
//! fields. Pairs with `#[union(boxed)]` to give a UNION enum variant a
//! boxed payload whose fields are laid out via the struct's declaration
//! order.

use heck::ToSnakeCase;
use proc_macro2::TokenStream as TokenStream2;
use quote::{ToTokens, format_ident, quote};
use syn::{
    Attribute, Data, DataStruct, DeriveInput, Fields, Ident, LitStr, Type, Variant,
};

pub(crate) fn expand_union_schema(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let enum_ident = &input.ident;

    let data = match &input.data {
        Data::Enum(e) => e,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "UnionSchema can only be derived on enums",
            ));
        }
    };

    let attrs = UnionAttrs::parse(&input.attrs)?;
    attrs.reject(&input.attrs, &["name"], "the enum")?;
    let type_name = attrs
        .name
        .clone()
        .unwrap_or_else(|| enum_ident.to_string().to_snake_case());

    let variants: Vec<ParsedVariant> = data
        .variants
        .iter()
        .map(ParsedVariant::from_syn)
        .collect::<syn::Result<_>>()?;
    if variants.len() > u8::MAX as usize + 1 {
        return Err(syn::Error::new_spanned(
            enum_ident,
            "UnionSchema: at most 256 variants supported (tag index is a u8)",
        ));
    }

    let variant_names: Vec<_> = variants.iter().map(|v| v.tag.as_str()).collect();
    let variant_field_lits: Vec<TokenStream2> = variants
        .iter()
        .map(|v| match &v.shape {
            VariantShape::Struct { fields } => {
                let names: Vec<String> = fields.iter().map(|f| f.ident.to_string()).collect();
                quote! { &[ #(#names),* ] }
            }
            VariantShape::Scalar { .. } => quote! { &[] },
            VariantShape::BoxedStruct { payload_ty } => quote! {
                <#payload_ty as ::diesel::turso::union::UnionStructPayload>::FIELD_NAMES
            },
        })
        .collect();
    let tag_index_arms = variants
        .iter()
        .enumerate()
        .map(|(i, v)| v.emit_tag_index_arm(i as u8));
    let encode_arms = variants.iter().map(|v| v.emit_encode_arm());
    let decode_arms = variants
        .iter()
        .enumerate()
        .map(|(i, v)| v.emit_decode_arm(enum_ident, i as u8));
    let create_type_sql = emit_create_type_sql(&type_name, &variants);
    let identifiers = emit_identifier_module(&input.vis, enum_ident, &variants)?;

    let tokens = quote! {
        #identifiers

        impl ::diesel::turso::union::UnionSchema for #enum_ident {
            fn type_name() -> &'static str { #type_name }
            fn variants() -> &'static [&'static str] { &[ #(#variant_names),* ] }
            fn variant_fields() -> &'static [&'static [&'static str]] {
                &[ #(#variant_field_lits),* ]
            }
            fn tag_index(&self) -> u8 {
                match self { #(#tag_index_arms)* }
            }
            fn encode_outer(
                &self,
            ) -> ::diesel::turso::union::EncodeResult<::diesel::turso::driver::Value> {
                match self { #(#encode_arms),* }
            }
            fn decode(
                __index: u8,
                __outer: ::diesel::turso::driver::Value,
            ) -> ::std::result::Result<Self, ::diesel::turso::union::DecodeError> {
                match __index {
                    #(#decode_arms)*
                    other => ::std::result::Result::Err(
                        ::diesel::turso::union::DecodeError::UnknownVariant {
                            index: other,
                            expected: <Self as ::diesel::turso::union::UnionSchema>::variants(),
                        },
                    ),
                }
            }
            fn create_type_sql() -> ::std::string::String { #create_type_sql }
        }

        impl ::diesel::serialize::ToSql<
            ::diesel::turso::union::TaggedUnion<#enum_ident>,
            ::diesel::turso::Turso,
        > for #enum_ident {
            fn to_sql(
                &self,
                __out: &mut ::diesel::serialize::Output<'_, '_, ::diesel::turso::Turso>,
            ) -> ::diesel::serialize::Result {
                ::diesel::turso::union::encode_for_bind(self, __out)
            }
        }

        impl ::diesel::deserialize::FromSql<
            ::diesel::turso::union::TaggedUnion<#enum_ident>,
            ::diesel::turso::Turso,
        > for #enum_ident {
            fn from_sql(
                __bytes: ::diesel::turso::TursoValue<'_>,
            ) -> ::diesel::deserialize::Result<Self> {
                ::diesel::turso::union::decode_from_blob::<Self>(__bytes)
            }
        }
    };

    Ok(tokens)
}

// ----- identifier modules --------------------------------------------------

/// The identifier module: one type per variant and per struct field, in a
/// `snake_case(EnumIdent)` module beside the enum, the way `table!` emits
/// one type per column.
///
/// It is what turns `struct_extract(union_extract(mid, 'telegram'),
/// 'chat_id')` from a string into `mid.extract(telegram::variant)
/// .field(telegram::chat_id)` — see `diesel::turso::union::expr`.
///
/// The module opens with `use super::*` so a field's Rust type can be named
/// the way the enum names it (`TgChatId`, a `use` alias in the enum's
/// module, resolves from a child module). Each field's SQL type is then a
/// hidden alias at the module's own level, so the per-variant submodules
/// never have to reach two scopes up for anything but their own generated
/// items.
fn emit_identifier_module(
    vis: &syn::Visibility,
    enum_ident: &Ident,
    variants: &[ParsedVariant],
) -> syn::Result<TokenStream2> {
    let mod_ident = format_ident!("{}", enum_ident.to_string().to_snake_case());
    let mod_doc = format!(
        "Identifier types for the `{enum_ident}` UNION, generated by \
         `#[derive(UnionSchema)]`.\n\n\
         One module per variant, holding a `variant` selector and — for a \
         struct variant — a `fields` marker and one type per field. Used \
         with `diesel::turso::union::{{UnionExpressionMethods, \
         CompositeExpressionMethods}}`:\n\n\
         ```ignore\n\
         col.extract({first}::variant).is_not_null()\n\
         col.extract({first}::variant).field({first}::some_field).eq(x)\n\
         ```",
        first = variants
            .first()
            .map(|v| v.tag.clone())
            .unwrap_or_else(|| "variant".into()),
    );

    let mut aliases: Vec<TokenStream2> = Vec::new();
    let mut variant_mods: Vec<TokenStream2> = Vec::new();

    for (index, variant) in variants.iter().enumerate() {
        let tag = &variant.tag;
        let tag_ident = syn::parse_str::<Ident>(tag).map_err(|_| {
            syn::Error::new_spanned(
                &variant.ident,
                format!(
                    "UnionSchema: tag {tag:?} is not a Rust identifier, so it cannot name \
                     the variant's module — spell a `#[union(tag = \"…\")]` that is"
                ),
            )
        })?;
        let tag_index = index as u8;

        let (payload, body) = match &variant.shape {
            VariantShape::Scalar { field } => {
                let alias = format_ident!("__sql_{}", tag_ident);
                let st = field.sql_type_tokens();
                aliases.push(quote! {
                    #[doc(hidden)]
                    pub type #alias = ::diesel::turso::union::NullableOf<#st>;
                });
                // A scalar variant has no fields, and `.field(…)` on one is
                // a compile error for the good reason that `struct_extract`
                // of a scalar is not a thing Turso can do.
                (quote! { super::#alias }, quote! {})
            }
            VariantShape::Struct { fields } => {
                let mut field_types = Vec::new();
                for (position, field) in fields.iter().enumerate() {
                    let field_ident = &field.ident;
                    let name = field_ident.to_string();
                    if name == "variant" || name == "fields" {
                        return Err(syn::Error::new_spanned(
                            field_ident,
                            format!(
                                "UnionSchema: a field named `{name}` would collide with the \
                                 generated `{name}` type in `{mod_ident}::{tag}` — rename it"
                            ),
                        ));
                    }
                    let alias = format_ident!("__sql_{}_{}", tag_ident, field_ident);
                    let st = field.sql_type_tokens();
                    aliases.push(quote! {
                        #[doc(hidden)]
                        pub type #alias = ::diesel::turso::union::NullableOf<#st>;
                    });
                    let doc = format!(
                        "`struct_extract(union_extract(<col>, '{tag}'), '{name}')`, \
                         as an expression."
                    );
                    field_types.push(quote! {
                        #[doc = #doc]
                        #[derive(Debug, Clone, Copy, Default, ::diesel::query_builder::QueryId)]
                        pub struct #field_ident;

                        impl ::diesel::turso::union::CompositeField for #field_ident {
                            type Shape = fields;
                            type SqlType = super::#alias;
                            const NAME: &'static str = #name;
                            const INDEX: usize = #position;
                        }
                    });
                }
                let names: Vec<String> = fields.iter().map(|f| f.ident.to_string()).collect();
                let fields_doc = format!(
                    "The field set of `{}`, the STRUCT behind the `{tag}` variant. \
                     Fields are typed against it, so a field of some other variant \
                     cannot be projected out of this one.",
                    variant.struct_type,
                );
                (
                    quote! { ::diesel::turso::union::NullableComposite<fields> },
                    quote! {
                        #[doc = #fields_doc]
                        #[derive(Debug, Clone, Copy, Default, ::diesel::query_builder::QueryId)]
                        pub struct fields;

                        impl ::diesel::turso::union::CompositeShape for fields {
                            const FIELD_NAMES: &'static [&'static str] = &[ #(#names),* ];
                        }

                        #(#field_types)*
                    },
                )
            }
            VariantShape::BoxedStruct { payload_ty } => {
                let alias = format_ident!("__payload_{}", tag_ident);
                aliases.push(quote! {
                    #[doc(hidden)]
                    pub type #alias = #payload_ty;
                });
                // A boxed variant's fields belong to the payload type, which
                // this derive can see only as a name — so the payload's own
                // `#[derive(UnionStructPayload)]` emits them, and the shape
                // marker here *is* the payload type. `.field(…)` works the
                // same; the field path is spelled from the payload's module.
                (
                    quote! { ::diesel::turso::union::NullableComposite<super::#alias> },
                    quote! {},
                )
            }
        };

        let variant_doc = format!(
            "`union_extract(<col>, '{tag}')` — the `{}` variant's payload, \
             NULL for a row holding any other variant.",
            variant.ident,
        );
        variant_mods.push(quote! {
            #[doc = #variant_doc]
            pub mod #tag_ident {
                #[doc = #variant_doc]
                #[derive(Debug, Clone, Copy, Default, ::diesel::query_builder::QueryId)]
                pub struct variant;

                impl ::diesel::turso::union::UnionVariant for variant {
                    type Union = super::__Union;
                    type Payload = #payload;
                    const TAG: u8 = #tag_index;
                    const TAG_NAME: &'static str = #tag;
                }

                #body
            }
        });
    }

    Ok(quote! {
        #[doc = #mod_doc]
        #[allow(non_camel_case_types, non_snake_case, unused_imports)]
        #vis mod #mod_ident {
            use super::*;

            /// What a `table!` column of this union is declared as.
            pub type SqlType = ::diesel::turso::union::TaggedUnion<super::#enum_ident>;

            #[doc(hidden)]
            pub type __Union = super::#enum_ident;

            #(#aliases)*

            #(#variant_mods)*
        }
    })
}

/// The identifier module for a `#[derive(UnionStructPayload)]` payload —
/// one type per field, addressing the payload wherever it is used as a
/// boxed variant's shape.
///
/// Same layout as a struct variant's submodule, one level shallower:
/// `signal_contact_payload::aci` rather than
/// `social_data::signal_contact::aci`. The payload type itself is the
/// composite marker, because it is the only name both derives share.
fn emit_payload_identifier_module(
    vis: &syn::Visibility,
    struct_ident: &Ident,
    fields: &[ParsedField],
) -> TokenStream2 {
    let mod_ident = format_ident!("{}", struct_ident.to_string().to_snake_case());
    let mod_doc = format!(
        "Identifier types for the fields of [`{struct_ident}`](super::{struct_ident}), \
         generated by `#[derive(UnionStructPayload)]`.\n\n\
         Reached through the boxed UNION variant that carries the payload:\n\n\
         ```ignore\n\
         col.extract(some_variant::variant).field({mod_ident}::some_field)\n\
         ```"
    );

    let mut aliases = Vec::new();
    let mut field_types = Vec::new();
    for (position, field) in fields.iter().enumerate() {
        let field_ident = &field.ident;
        let name = field_ident.to_string();
        let alias = format_ident!("__sql_{}", field_ident);
        let st = field.sql_type_tokens();
        aliases.push(quote! {
            #[doc(hidden)]
            pub type #alias = ::diesel::turso::union::NullableOf<#st>;
        });
        let doc = format!("`struct_extract(<payload>, '{name}')`, as an expression.");
        field_types.push(quote! {
            #[doc = #doc]
            #[derive(Debug, Clone, Copy, Default, ::diesel::query_builder::QueryId)]
            pub struct #field_ident;

            impl ::diesel::turso::union::CompositeField for #field_ident {
                type Shape = super::#struct_ident;
                type SqlType = #alias;
                const NAME: &'static str = #name;
                const INDEX: usize = #position;
            }
        });
    }

    quote! {
        #[doc = #mod_doc]
        #[allow(non_camel_case_types, non_snake_case, unused_imports)]
        #vis mod #mod_ident {
            use super::*;

            #(#aliases)*
            #(#field_types)*
        }
    }
}

// ----- parsing -------------------------------------------------------------

enum VariantShape {
    /// `Variant(T)` — scalar; outer column is the scalar itself. Boxed
    /// because a `ParsedField` holds two `syn::Type`s and the other two
    /// shapes are a `Vec` and a single type.
    Scalar { field: Box<ParsedField> },
    /// `Variant { f1: T1, f2: T2, … }` — struct; outer column is a BLOB.
    Struct { fields: Vec<ParsedField> },
    /// `#[union(boxed)] Variant(Box<T>)` — struct wire format, but the
    /// enum holds `Box<T>` so the variant is one pointer wide. `T` must
    /// implement `UnionStructPayload`.
    BoxedStruct { payload_ty: Box<syn::Type> },
}

struct ParsedField {
    ident: Ident,
    ty: syn::Type,
    /// `#[union(sql_type = …)]`, when the type's `TursoFieldType` default
    /// is not what this field wants.
    sql_type: Option<syn::Type>,
}

impl ParsedField {
    /// The SQL type this field encodes through — the override if there is
    /// one, otherwise the Rust type's declared default.
    fn sql_type_tokens(&self) -> TokenStream2 {
        match &self.sql_type {
            Some(st) => quote! { #st },
            None => {
                let ty = &self.ty;
                quote! { <#ty as ::diesel::turso::union::TursoFieldType>::SqlType }
            }
        }
    }
}

struct ParsedVariant {
    ident: Ident,
    tag: String,
    /// Name of this variant's `CREATE TYPE … AS STRUCT(…)`. Meaningless
    /// (and unset) for scalar variants.
    struct_type: String,
    shape: VariantShape,
}

impl ParsedVariant {
    fn from_syn(v: &Variant) -> syn::Result<Self> {
        let ident = v.ident.clone();
        let attrs = UnionAttrs::parse(&v.attrs)?;
        attrs.reject(&v.attrs, &["tag", "struct_type", "boxed"], "a variant")?;
        let tag = attrs
            .tag
            .clone()
            .unwrap_or_else(|| ident.to_string().to_snake_case());
        let struct_type = attrs
            .struct_type
            .clone()
            .unwrap_or_else(|| format!("{tag}_t"));
        let shape = match &v.fields {
            Fields::Unit => {
                return Err(syn::Error::new_spanned(
                    v,
                    "UnionSchema: unit variants aren't supported — use \
                     Variant(SomeType) for scalar variants or \
                     Variant { field: Ty } for struct variants",
                ));
            }
            Fields::Unnamed(u) if u.unnamed.len() == 1 => {
                let only = u.unnamed.first().expect("len == 1 checked above");
                let ty = only.ty.clone();
                if attrs.boxed {
                    let payload_ty = extract_box_inner(&ty).ok_or_else(|| {
                        syn::Error::new_spanned(
                            &ty,
                            "UnionSchema: #[union(boxed)] requires a Box<T> payload",
                        )
                    })?;
                    VariantShape::BoxedStruct {
                        payload_ty: Box::new(payload_ty),
                    }
                } else {
                    if attrs.struct_type.is_some() {
                        return Err(syn::Error::new_spanned(
                            v,
                            "UnionSchema: #[union(struct_type = …)] names a STRUCT type, \
                             but this is a scalar variant — its UNION entry is a storage \
                             class, not a named type",
                        ));
                    }
                    let field_attrs = UnionAttrs::parse(&only.attrs)?;
                    field_attrs.reject(&only.attrs, &["sql_type"], "a field")?;
                    VariantShape::Scalar {
                        field: Box::new(ParsedField {
                            ident: format_ident!("__v"),
                            ty,
                            sql_type: field_attrs.sql_type,
                        }),
                    }
                }
            }
            Fields::Unnamed(_) => {
                return Err(syn::Error::new_spanned(
                    v,
                    "UnionSchema: multi-field tuple variants aren't supported — \
                     use named fields (Variant { a: Ta, b: Tb }) to disambiguate",
                ));
            }
            Fields::Named(named) => {
                if attrs.boxed {
                    return Err(syn::Error::new_spanned(
                        v,
                        "UnionSchema: #[union(boxed)] only applies to scalar-shape \
                         variants with a Box<T> payload; struct variants are already \
                         flat on the Rust side",
                    ));
                }
                VariantShape::Struct {
                    fields: named
                        .named
                        .iter()
                        .map(ParsedField::from_syn)
                        .collect::<syn::Result<_>>()?,
                }
            }
        };
        Ok(Self {
            ident,
            tag,
            struct_type,
            shape,
        })
    }

    fn emit_tag_index_arm(&self, idx: u8) -> TokenStream2 {
        let ident = &self.ident;
        match self.shape {
            VariantShape::Scalar { .. } | VariantShape::BoxedStruct { .. } => {
                quote! { Self::#ident(..) => #idx, }
            }
            VariantShape::Struct { .. } => quote! { Self::#ident { .. } => #idx, },
        }
    }

    fn emit_encode_arm(&self) -> TokenStream2 {
        let ident = &self.ident;
        match &self.shape {
            VariantShape::Scalar { field } => {
                let st = field.sql_type_tokens();
                quote! {
                    Self::#ident(__v) => ::diesel::turso::union::encode_field::<#st, _>(__v)
                }
            }
            VariantShape::Struct { fields } => {
                let binds: Vec<_> = fields.iter().map(|f| &f.ident).collect();
                let field_count = fields.len();
                let pushes = fields.iter().map(|f| {
                    let fi = &f.ident;
                    let st = f.sql_type_tokens();
                    quote! {
                        __inner.push(::diesel::turso::union::encode_field::<#st, _>(#fi)?);
                    }
                });
                quote! {
                    Self::#ident { #(#binds),* } => {
                        let mut __inner: ::std::vec::Vec<::diesel::turso::driver::Value> =
                            ::std::vec::Vec::with_capacity(#field_count);
                        #(#pushes)*
                        ::std::result::Result::Ok(::diesel::turso::driver::Value::Blob(
                            ::diesel::turso::union::encode_record(&__inner),
                        ))
                    }
                }
            }
            VariantShape::BoxedStruct { payload_ty } => quote! {
                Self::#ident(__payload) => {
                    let __inner = <#payload_ty as ::diesel::turso::union::UnionStructPayload>
                        ::encode_fields(&**__payload)?;
                    ::std::result::Result::Ok(::diesel::turso::driver::Value::Blob(
                        ::diesel::turso::union::encode_record(&__inner),
                    ))
                }
            },
        }
    }

    fn emit_decode_arm(&self, enum_ident: &Ident, idx: u8) -> TokenStream2 {
        let tag = &self.tag;
        let ident = &self.ident;
        match &self.shape {
            VariantShape::Scalar { field } => {
                let ty = &field.ty;
                let st = field.sql_type_tokens();
                quote! {
                    #idx => {
                        let __value = ::diesel::turso::union::decode_field::<#st, #ty>(&__outer)
                            .map_err(|e| ::diesel::turso::union::DecodeError::Field {
                                variant: #tag,
                                field: "0",
                                message: ::std::string::ToString::to_string(&e),
                            })?;
                        ::std::result::Result::Ok(#enum_ident::#ident(__value))
                    }
                }
            }
            VariantShape::Struct { fields } => {
                let field_count = fields.len();
                let per_field = fields
                    .iter()
                    .map(|f| {
                        let fi = &f.ident;
                        let ty = &f.ty;
                        let st = f.sql_type_tokens();
                        let name = fi.to_string();
                        let var = format_ident!("__field_{}", fi);
                        quote! {
                            // Missing fields are caught by the FieldCount
                            // guard below, so this .next() always has a value
                            // when we reach it.
                            let #var = ::diesel::turso::union::decode_field::<#st, #ty>(
                                &__it.next().expect("field count already validated"),
                            ).map_err(|e| ::diesel::turso::union::DecodeError::Field {
                                variant: #tag,
                                field: #name,
                                message: ::std::string::ToString::to_string(&e),
                            })?;
                        }
                    })
                    .collect::<Vec<_>>();
                let field_builds = fields.iter().map(|f| {
                    let fi = &f.ident;
                    let var = format_ident!("__field_{}", fi);
                    quote! { #fi: #var }
                });
                quote! {
                    #idx => {
                        let __inner_blob = match __outer {
                            ::diesel::turso::driver::Value::Blob(b) => b,
                            other => return ::std::result::Result::Err(
                                ::diesel::turso::union::DecodeError::TypeMismatch {
                                    expected: concat!("variant ", #tag, " struct BLOB"),
                                    got_kind: ::diesel::turso::union::ValueKind::of(&other),
                                },
                            ),
                        };
                        let __fields = ::diesel::turso::union::decode_record(&__inner_blob)?;
                        if __fields.len() != #field_count {
                            return ::std::result::Result::Err(
                                ::diesel::turso::union::DecodeError::FieldCount {
                                    variant: #tag,
                                    expected: #field_count,
                                    got: __fields.len(),
                                },
                            );
                        }
                        let mut __it = __fields.into_iter();
                        #(#per_field)*
                        ::std::result::Result::Ok(#enum_ident::#ident { #(#field_builds),* })
                    }
                }
            }
            VariantShape::BoxedStruct { payload_ty } => quote! {
                #idx => {
                    let __inner_blob = match __outer {
                        ::diesel::turso::driver::Value::Blob(b) => b,
                        other => return ::std::result::Result::Err(
                            ::diesel::turso::union::DecodeError::TypeMismatch {
                                expected: concat!("variant ", #tag, " struct BLOB"),
                                got_kind: ::diesel::turso::union::ValueKind::of(&other),
                            },
                        ),
                    };
                    let __fields = ::diesel::turso::union::decode_record(&__inner_blob)?;
                    let __payload = <#payload_ty as ::diesel::turso::union::UnionStructPayload>
                        ::decode_fields(__fields)?;
                    ::std::result::Result::Ok(
                        #enum_ident::#ident(::std::boxed::Box::new(__payload)),
                    )
                }
            },
        }
    }

    /// The `CREATE TYPE <struct_type> AS STRUCT(…)` this variant needs, or
    /// `None` for a scalar variant (whose UNION entry is a bare storage
    /// class).
    fn emit_struct_decl(&self) -> Option<TokenStream2> {
        let name = &self.struct_type;
        match &self.shape {
            VariantShape::Scalar { .. } => None,
            VariantShape::Struct { fields } => {
                let field_frags = fields.iter().map(|f| {
                    let st = f.sql_type_tokens();
                    let name = f.ident.to_string();
                    quote! {
                        ::std::format!(
                            "{} {}",
                            #name,
                            ::diesel::turso::union::ddl_type_name::<#st>(),
                        )
                    }
                });
                Some(quote! {
                    {
                        let __fields: ::std::vec::Vec<::std::string::String> =
                            ::std::vec![ #(#field_frags),* ];
                        ::std::format!(
                            "CREATE TYPE {name} AS STRUCT({fields})",
                            name = #name,
                            fields = __fields.join(", "),
                        )
                    }
                })
            }
            VariantShape::BoxedStruct { payload_ty } => Some(quote! {
                {
                    let __fields: ::std::vec::Vec<::std::string::String> =
                        <#payload_ty as ::diesel::turso::union::UnionStructPayload>
                            ::sql_struct_fields()
                            .into_iter()
                            .map(|(n, t)| ::std::format!("{n} {t}"))
                            .collect();
                    ::std::format!(
                        "CREATE TYPE {name} AS STRUCT({fields})",
                        name = #name,
                        fields = __fields.join(", "),
                    )
                }
            }),
        }
    }

    /// This variant's entry in the `AS UNION(…)` list.
    fn emit_union_entry(&self) -> TokenStream2 {
        let tag = &self.tag;
        match &self.shape {
            VariantShape::Scalar { field } => {
                let st = field.sql_type_tokens();
                quote! {
                    ::std::format!(
                        "{tag} {sql_ty}",
                        tag = #tag,
                        sql_ty = ::diesel::turso::union::ddl_type_name::<#st>(),
                    )
                }
            }
            VariantShape::Struct { .. } | VariantShape::BoxedStruct { .. } => {
                let name = &self.struct_type;
                quote! { ::std::format!("{tag} {name}", tag = #tag, name = #name) }
            }
        }
    }
}

impl ParsedField {
    fn from_syn(f: &syn::Field) -> syn::Result<Self> {
        let attrs = UnionAttrs::parse(&f.attrs)?;
        attrs.reject(&f.attrs, &["sql_type"], "a field")?;
        Ok(Self {
            ident: f.ident.clone().expect("named field"),
            ty: f.ty.clone(),
            sql_type: attrs.sql_type,
        })
    }
}

fn emit_create_type_sql(type_name: &str, variants: &[ParsedVariant]) -> TokenStream2 {
    let struct_decls = variants.iter().filter_map(|v| v.emit_struct_decl());
    let union_entries = variants.iter().map(|v| v.emit_union_entry());
    quote! {
        {
            // Deduplicated because several variants may share one struct
            // type — `whatsapp_contact`, `whatsapp_group` and
            // `whatsapp_newsletter` are all `whatsapp_dialog_id` — and
            // running the same CREATE TYPE twice is an error. Two variants
            // naming one type with *different* field lists produce two
            // conflicting statements rather than a silent overwrite, so
            // the golden test sees it.
            let __decls: ::std::vec::Vec<::std::string::String> =
                ::std::vec![ #(#struct_decls),* ];
            let mut __structs: ::std::vec::Vec<::std::string::String> = ::std::vec::Vec::new();
            for __decl in __decls {
                if !__structs.contains(&__decl) {
                    __structs.push(__decl);
                }
            }
            let __variants: ::std::vec::Vec<::std::string::String> =
                ::std::vec![ #(#union_entries),* ];
            let mut __sql = ::std::string::String::new();
            for __decl in &__structs {
                __sql.push_str(__decl);
                __sql.push_str("; ");
            }
            __sql.push_str(&::std::format!(
                "CREATE TYPE {name} AS UNION({variants})",
                name = #type_name,
                variants = __variants.join(", "),
            ));
            __sql
        }
    }
}

// ----- attribute parsing ---------------------------------------------------

/// Everything `#[union(...)]` can say, wherever it is written. Parsed in
/// one pass so a typo'd key is an error once rather than being silently
/// consumed by whichever reader ran first.
#[derive(Default)]
struct UnionAttrs {
    name: Option<String>,
    tag: Option<String>,
    struct_type: Option<String>,
    sql_type: Option<syn::Type>,
    boxed: bool,
}

impl UnionAttrs {
    fn parse(attrs: &[Attribute]) -> syn::Result<Self> {
        let mut out = Self::default();
        for attr in attrs.iter().filter(|a| a.path().is_ident("union")) {
            attr.parse_nested_meta(|meta| {
                if meta.path.is_ident("name") {
                    out.name = Some(meta.value()?.parse::<LitStr>()?.value());
                } else if meta.path.is_ident("tag") {
                    out.tag = Some(meta.value()?.parse::<LitStr>()?.value());
                } else if meta.path.is_ident("struct_type") {
                    out.struct_type = Some(meta.value()?.parse::<LitStr>()?.value());
                } else if meta.path.is_ident("sql_type") {
                    out.sql_type = Some(meta.value()?.parse::<Type>()?);
                } else if meta.path.is_ident("boxed") {
                    out.boxed = true;
                } else {
                    return Err(meta.error(format!("unknown #[union] key: {:?}", meta.path)));
                }
                Ok(())
            })?;
        }
        Ok(out)
    }

    /// Error on any key that is set but not in `allowed` — so
    /// `#[union(name = "…")]` on a variant, which would silently do
    /// nothing, fails the build instead.
    fn reject(&self, attrs: &[Attribute], allowed: &[&str], position: &str) -> syn::Result<()> {
        let set = [
            ("name", self.name.is_some()),
            ("tag", self.tag.is_some()),
            ("struct_type", self.struct_type.is_some()),
            ("sql_type", self.sql_type.is_some()),
            ("boxed", self.boxed),
        ];
        for (key, present) in set {
            if present && !allowed.contains(&key) {
                let span = attrs
                    .iter()
                    .find(|a| a.path().is_ident("union"))
                    .map(|a| a.to_token_stream())
                    .unwrap_or_default();
                return Err(syn::Error::new_spanned(
                    span,
                    format!("UnionSchema: #[union({key} = …)] is not accepted on {position}"),
                ));
            }
        }
        Ok(())
    }
}

/// If `ty` is `Box<T>` (resolved by path ident, not by full path — matches
/// `std::boxed::Box<T>`, `alloc::boxed::Box<T>`, or a bare `Box<T>`), return
/// the inner `T`. Otherwise `None`.
fn extract_box_inner(ty: &Type) -> Option<Type> {
    let Type::Path(tp) = ty else {
        return None;
    };
    let last = tp.path.segments.last()?;
    if last.ident != "Box" {
        return None;
    }
    let syn::PathArguments::AngleBracketed(args) = &last.arguments else {
        return None;
    };
    for arg in &args.args {
        if let syn::GenericArgument::Type(inner) = arg {
            return Some(inner.clone());
        }
    }
    None
}

pub(crate) fn expand_struct_payload(input: &DeriveInput) -> syn::Result<TokenStream2> {
    let struct_ident = &input.ident;
    let data = match &input.data {
        Data::Struct(DataStruct {
            fields: Fields::Named(named),
            ..
        }) => named,
        _ => {
            return Err(syn::Error::new_spanned(
                &input.ident,
                "UnionStructPayload can only be derived on structs with named fields",
            ));
        }
    };

    let fields: Vec<ParsedField> = data
        .named
        .iter()
        .map(ParsedField::from_syn)
        .collect::<syn::Result<_>>()?;

    let field_name_lits: Vec<String> = fields.iter().map(|f| f.ident.to_string()).collect();
    let field_count = fields.len();
    let struct_name_lit = struct_ident.to_string();

    let encode_pushes = fields.iter().map(|f| {
        let fi = &f.ident;
        let st = f.sql_type_tokens();
        quote! {
            __out.push(::diesel::turso::union::encode_field::<#st, _>(&self.#fi)?);
        }
    });

    let decode_fields = fields.iter().map(|f| {
        let fi = &f.ident;
        let ty = &f.ty;
        let st = f.sql_type_tokens();
        let name = fi.to_string();
        let var = format_ident!("__field_{}", fi);
        quote! {
            let #var = ::diesel::turso::union::decode_field::<#st, #ty>(
                &__it.next().expect("field count already validated"),
            ).map_err(|e| ::diesel::turso::union::DecodeError::Field {
                variant: #struct_name_lit,
                field: #name,
                message: ::std::string::ToString::to_string(&e),
            })?;
        }
    });
    let decode_builds = fields.iter().map(|f| {
        let fi = &f.ident;
        let var = format_ident!("__field_{}", fi);
        quote! { #fi: #var }
    });

    let sql_rows = fields.iter().map(|f| {
        let st = f.sql_type_tokens();
        let name = f.ident.to_string();
        quote! { (#name, ::diesel::turso::union::ddl_type_name::<#st>()) }
    });

    let identifiers = emit_payload_identifier_module(&input.vis, struct_ident, &fields);

    Ok(quote! {
        #identifiers

        // The payload type doubles as the composite marker its fields are
        // typed against — see `emit_payload_identifier_module`.
        impl ::diesel::turso::union::CompositeShape for #struct_ident {
            const FIELD_NAMES: &'static [&'static str] =
                <Self as ::diesel::turso::union::UnionStructPayload>::FIELD_NAMES;
        }

        impl ::diesel::turso::union::UnionStructPayload for #struct_ident {
            const FIELD_NAMES: &'static [&'static str] = &[ #(#field_name_lits),* ];

            fn sql_struct_fields() -> ::std::vec::Vec<(&'static str, &'static str)> {
                ::std::vec![ #(#sql_rows),* ]
            }

            fn encode_fields(
                &self,
            ) -> ::diesel::turso::union::EncodeResult<::std::vec::Vec<::diesel::turso::driver::Value>> {
                let mut __out: ::std::vec::Vec<::diesel::turso::driver::Value> =
                    ::std::vec::Vec::with_capacity(#field_count);
                #(#encode_pushes)*
                ::std::result::Result::Ok(__out)
            }

            fn decode_fields(
                __values: ::std::vec::Vec<::diesel::turso::driver::Value>,
            ) -> ::std::result::Result<Self, ::diesel::turso::union::DecodeError> {
                if __values.len() != #field_count {
                    return ::std::result::Result::Err(
                        ::diesel::turso::union::DecodeError::FieldCount {
                            variant: #struct_name_lit,
                            expected: #field_count,
                            got: __values.len(),
                        },
                    );
                }
                let mut __it = __values.into_iter();
                #(#decode_fields)*
                ::std::result::Result::Ok(Self { #(#decode_builds),* })
            }
        }
    })
}
