use proc_macro::TokenStream;
use quote::{format_ident, quote};
use syn::{
    parse_macro_input, parse_quote, Attribute, Data, DataEnum, DataStruct, DeriveInput, Fields,
    Generics, LitStr, Path, Type,
};

#[derive(Default)]
struct Attrs {
    rename: Option<String>,
    rename_all: Option<String>,
    skip: bool,
    skip_to_term: bool,
    skip_from_term: bool,
    flatten: bool,
    default: Option<Option<Path>>,
    untagged: bool,
    tag: Option<String>,
    content: Option<String>,
    crate_path: Option<Path>,
}

fn attrs(attrs: &[Attribute]) -> syn::Result<Attrs> {
    let mut out = Attrs::default();
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("splint")) {
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("rename") {
                out.rename = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("rename_all") {
                out.rename_all = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("skip") {
                out.skip = true;
            } else if meta.path.is_ident("skip_to_term") {
                out.skip_to_term = true;
            } else if meta.path.is_ident("skip_from_term") {
                out.skip_from_term = true;
            } else if meta.path.is_ident("flatten") {
                out.flatten = true;
            } else if meta.path.is_ident("default") {
                if meta.input.peek(syn::Token![=]) {
                    let path = meta.value()?.parse::<LitStr>()?.parse()?;
                    out.default = Some(Some(path));
                } else {
                    out.default = Some(None);
                }
            } else if meta.path.is_ident("untagged") {
                out.untagged = true;
            } else if meta.path.is_ident("tag") {
                out.tag = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("content") {
                out.content = Some(meta.value()?.parse::<LitStr>()?.value());
            } else if meta.path.is_ident("crate") {
                out.crate_path = Some(meta.value()?.parse::<LitStr>()?.parse()?);
            } else {
                return Err(meta.error("unsupported #[splint] attribute"));
            }
            Ok(())
        })?;
    }
    Ok(out)
}

fn rename(name: &str, rule: Option<&str>) -> String {
    let words = split_words(name);
    match rule {
        Some("lowercase") => words.concat().to_lowercase(),
        Some("UPPERCASE") => words.concat().to_uppercase(),
        Some("snake_case") => words.join("_").to_lowercase(),
        Some("SCREAMING_SNAKE_CASE") => words.join("_").to_uppercase(),
        Some("kebab-case") => words.join("-").to_lowercase(),
        Some("SCREAMING-KEBAB-CASE") => words.join("-").to_uppercase(),
        Some("camelCase") => {
            let mut it = words.into_iter();
            let first = it.next().unwrap_or_default().to_lowercase();
            first + &it.map(capitalize).collect::<String>()
        }
        Some("PascalCase") => words.into_iter().map(capitalize).collect(),
        _ => name.to_owned(),
    }
}

fn split_words(value: &str) -> Vec<String> {
    let mut result = Vec::new();
    let mut current = String::new();
    for ch in value.chars() {
        if ch == '_' || ch == '-' {
            if !current.is_empty() {
                result.push(std::mem::take(&mut current));
            }
        } else if ch.is_uppercase() && !current.is_empty() {
            result.push(std::mem::take(&mut current));
            current.push(ch);
        } else {
            current.push(ch);
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    result
}

fn capitalize(value: String) -> String {
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => first.to_uppercase().collect::<String>() + &chars.as_str().to_lowercase(),
        None => String::new(),
    }
}

fn crate_path(container: &Attrs) -> Path {
    container
        .crate_path
        .clone()
        .unwrap_or_else(|| parse_quote!(::splint))
}

fn add_bound(mut generics: Generics, tys: &[Type], bound: proc_macro2::TokenStream) -> Generics {
    let clause = generics.make_where_clause();
    for ty in tys {
        clause.predicates.push(parse_quote!(#ty: #bound));
    }
    generics
}

fn add_predicates(mut generics: Generics, predicates: Vec<proc_macro2::TokenStream>) -> Generics {
    let clause = generics.make_where_clause();
    for predicate in predicates {
        clause.predicates.push(parse_quote!(#predicate));
    }
    generics
}

#[proc_macro_derive(ToTerm, attributes(splint))]
pub fn derive_to_term(input: TokenStream) -> TokenStream {
    match expand_to(parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

#[proc_macro_derive(FromTerm, attributes(splint))]
pub fn derive_from_term(input: TokenStream) -> TokenStream {
    match expand_from(parse_macro_input!(input as DeriveInput)) {
        Ok(tokens) => tokens.into(),
        Err(error) => error.into_compile_error().into(),
    }
}

fn expand_to(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let container = attrs(&input.attrs)?;
    let krate = crate_path(&container);
    match &input.data {
        Data::Struct(data) => struct_to(&input, data, &container, &krate),
        Data::Enum(data) => enum_to(&input, data, &container, &krate),
        Data::Union(_) => Err(syn::Error::new_spanned(
            input,
            "ToTerm cannot be derived for unions",
        )),
    }
}

fn expand_from(input: DeriveInput) -> syn::Result<proc_macro2::TokenStream> {
    let container = attrs(&input.attrs)?;
    let krate = crate_path(&container);
    match &input.data {
        Data::Struct(data) => struct_from(&input, data, &container, &krate),
        Data::Enum(data) => enum_from(&input, data, &container, &krate),
        Data::Union(_) => Err(syn::Error::new_spanned(
            input,
            "FromTerm cannot be derived for unions",
        )),
    }
}

fn struct_to(
    input: &DeriveInput,
    data: &DataStruct,
    container: &Attrs,
    krate: &Path,
) -> syn::Result<proc_macro2::TokenStream> {
    let ident = &input.ident;
    let tag = container
        .rename
        .clone()
        .unwrap_or_else(|| ident.to_string());
    match &data.fields {
        Fields::Named(fields) => {
            let mut bounds = Vec::new();
            let mut writes = Vec::new();
            for field in &fields.named {
                let name = field.ident.as_ref().unwrap();
                let a = attrs(&field.attrs)?;
                if a.skip || a.skip_to_term {
                    continue;
                }
                let ty = field.ty.clone();
                if a.flatten {
                    bounds.push(quote!(#ty: #krate::codec::ToTermFields));
                    writes.push(quote! { __fields.extend(#krate::codec::ToTermFields::__to_fields(&self.#name, __ctx)?); });
                } else {
                    bounds.push(quote!(#ty: #krate::ToTerm));
                    let wire = a.rename.unwrap_or_else(|| {
                        rename(&name.to_string(), container.rename_all.as_deref())
                    });
                    writes.push(quote! { let __term=__ctx.term()?; if #krate::ToTerm::__to_field(&self.#name,__ctx,__term)? { __fields.push((#wire.to_owned(),__term)); } });
                }
            }
            let generics = add_predicates(input.generics.clone(), bounds);
            let (ig, tg, wc) = generics.split_for_impl();
            Ok(quote! {
                impl #ig #krate::codec::ToTermFields for #ident #tg #wc {
                    fn __to_fields<'a,__C:#krate::FliContext+?Sized>(&self,__ctx:&'a __C)->Result<Vec<(String,#krate::Term<'a>)>,#krate::TermCodecError>{let mut __fields=Vec::new();#(#writes)* Ok(__fields)}
                }
                impl #ig #krate::ToTerm for #ident #tg #wc {
                    fn to_term<__C:#krate::FliContext+?Sized>(&self,__ctx:&__C,__dest:#krate::Term<'_>)->Result<(),#krate::TermCodecError>{#krate::codec::put_dict(__ctx,__dest,#tag,#krate::codec::ToTermFields::__to_fields(self,__ctx)?) }
                }
            })
        }
        Fields::Unnamed(fields) => {
            let tys: Vec<_> = fields.unnamed.iter().map(|f| f.ty.clone()).collect();
            let generics = add_bound(input.generics.clone(), &tys, quote!(#krate::ToTerm));
            let (ig, tg, wc) = generics.split_for_impl();
            let writes=(0..tys.len()).map(|i|{let idx=syn::Index::from(i);quote!{let __term=__ctx.term()?;#krate::ToTerm::to_term(&self.#idx,__ctx,__term)?;__values.push(__term);}});
            Ok(
                quote! { impl #ig #krate::ToTerm for #ident #tg #wc { fn to_term<__C:#krate::FliContext+?Sized>(&self,__ctx:&__C,__dest:#krate::Term<'_>)->Result<(),#krate::TermCodecError>{let mut __values=Vec::new();#(#writes)*#krate::codec::put_compound(__ctx,__dest,#tag,&__values)} } },
            )
        }
        Fields::Unit => {
            let (ig, tg, wc) = input.generics.split_for_impl();
            Ok(
                quote! { impl #ig #krate::ToTerm for #ident #tg #wc { fn to_term<__C:#krate::FliContext+?Sized>(&self,_:&__C,_:#krate::Term<'_>)->Result<(),#krate::TermCodecError>{Err(#krate::TermCodecError::OptionOutsideField)} fn __to_field<__C:#krate::FliContext+?Sized>(&self,_:&__C,_:#krate::Term<'_>)->Result<bool,#krate::TermCodecError>{Ok(false)} } },
            )
        }
    }
}

fn struct_from(
    input: &DeriveInput,
    data: &DataStruct,
    container: &Attrs,
    krate: &Path,
) -> syn::Result<proc_macro2::TokenStream> {
    let ident = &input.ident;
    if matches!(container.default, Some(Some(_))) {
        return Err(syn::Error::new_spanned(
            input,
            "container-level default does not accept a function path",
        ));
    }
    let tag = container
        .rename
        .clone()
        .unwrap_or_else(|| ident.to_string());
    match &data.fields {
        Fields::Named(fields) => {
            let mut bounds = Vec::new();
            let mut reads = Vec::new();
            let mut names = Vec::new();
            for field in &fields.named {
                let name = field.ident.as_ref().unwrap();
                names.push(name);
                let a = attrs(&field.attrs)?;
                let ty = field.ty.clone();
                if a.skip || a.skip_from_term {
                    reads.push(quote! {let #name=::core::default::Default::default();});
                    continue;
                }
                if a.flatten {
                    bounds.push(quote!(#ty: #krate::codec::FromTermFields));
                    reads.push(quote!{let #name=#krate::codec::FromTermFields::__from_fields(__ctx,__fields)?;});
                } else {
                    bounds.push(quote!(#ty: #krate::FromTerm));
                    let wire = a.rename.unwrap_or_else(|| {
                        rename(&name.to_string(), container.rename_all.as_deref())
                    });
                    let field_default = a.default.or_else(|| {
                        if container.default.is_some() {
                            Some(None)
                        } else {
                            None
                        }
                    });
                    let expr = match field_default {
                        Some(Some(path)) => {
                            quote! {match __fields.remove(#wire){Some(t)=>#krate::FromTerm::from_term(__ctx,t)?,None=>#path()}}
                        }
                        Some(None) => {
                            quote! {match __fields.remove(#wire){Some(t)=>#krate::FromTerm::from_term(__ctx,t)?,None=>::core::default::Default::default()}}
                        }
                        None => {
                            quote! {#krate::FromTerm::__from_field(__ctx,__fields.remove(#wire),#wire)?}
                        }
                    };
                    reads.push(quote! {let #name:#ty=#expr;});
                }
            }
            let generics = add_predicates(input.generics.clone(), bounds);
            let (ig, tg, wc) = generics.split_for_impl();
            Ok(
                quote! {impl #ig #krate::codec::FromTermFields for #ident #tg #wc{fn __from_fields<'a,__C:#krate::FliContext+?Sized>(__ctx:&'a __C,__fields:&mut ::std::collections::BTreeMap<String,#krate::Term<'a>>)->Result<Self,#krate::TermCodecError>{#(#reads)*Ok(Self{#(#names),*})}} impl #ig #krate::FromTerm for #ident #tg #wc{fn from_term<__C:#krate::FliContext+?Sized>(__ctx:&__C,__term:#krate::Term<'_>)->Result<Self,#krate::TermCodecError>{#krate::codec::require_dict_tag(__ctx,__term,#tag)?;let mut __fields=#krate::codec::dict_fields(__ctx,__term)?;#krate::codec::FromTermFields::__from_fields(__ctx,&mut __fields)}}},
            )
        }
        Fields::Unnamed(fields) => {
            let tys: Vec<_> = fields.unnamed.iter().map(|f| f.ty.clone()).collect();
            let arity = tys.len();
            let generics = add_bound(input.generics.clone(), &tys, quote!(#krate::FromTerm));
            let (ig, tg, wc) = generics.split_for_impl();
            let vals = (0..arity).map(|i| {
                let idx = syn::Index::from(i);
                quote! {#krate::FromTerm::from_term(__ctx,__args[#idx])?}
            });
            Ok(
                quote! {impl #ig #krate::FromTerm for #ident #tg #wc{fn from_term<__C:#krate::FliContext+?Sized>(__ctx:&__C,__term:#krate::Term<'_>)->Result<Self,#krate::TermCodecError>{let __args=#krate::codec::compound_args(__ctx,__term,#tag,#arity)?;Ok(Self(#(#vals),*))}}},
            )
        }
        Fields::Unit => {
            let (ig, tg, wc) = input.generics.split_for_impl();
            Ok(
                quote! {impl #ig #krate::FromTerm for #ident #tg #wc{fn from_term<__C:#krate::FliContext+?Sized>(_:&__C,_:#krate::Term<'_>)->Result<Self,#krate::TermCodecError>{Err(#krate::TermCodecError::OptionOutsideField)}fn __from_field<__C:#krate::FliContext+?Sized>(_:&__C,_:Option<#krate::Term<'_>>,_:&str)->Result<Self,#krate::TermCodecError>{Ok(Self)}}},
            )
        }
    }
}

fn enum_to(
    input: &DeriveInput,
    data: &DataEnum,
    container: &Attrs,
    krate: &Path,
) -> syn::Result<proc_macro2::TokenStream> {
    if container.tag.is_some() {
        return tagged_enum_to(input, data, container, krate);
    }
    let ident = &input.ident;
    let mut tys = Vec::new();
    let mut arms = Vec::new();
    for variant in &data.variants {
        let va = attrs(&variant.attrs)?;
        if va.skip || va.skip_to_term {
            let vi = &variant.ident;
            let pattern = match &variant.fields {
                Fields::Unit => quote!(Self::#vi),
                Fields::Unnamed(_) => quote!(Self::#vi(..)),
                Fields::Named(_) => quote!(Self::#vi { .. }),
            };
            arms.push(quote! { #pattern => Err(#krate::TermCodecError::Message(
                concat!("variant ", stringify!(#vi), " is skipped for ToTerm").to_owned()
            )) });
            continue;
        }
        let vi = &variant.ident;
        let wire = va
            .rename
            .unwrap_or_else(|| rename(&vi.to_string(), container.rename_all.as_deref()));
        match &variant.fields {
            Fields::Unit if container.untagged => arms.push(quote! {Self::#vi=>Err(
                #krate::TermCodecError::OptionOutsideField
            )}),
            Fields::Unit => arms.push(quote! {Self::#vi=>{__dest.put_atom_text(#wire)?;Ok(())}}),
            Fields::Unnamed(fields) => {
                let binds: Vec<_> = (0..fields.unnamed.len())
                    .map(|i| format_ident!("__v{i}"))
                    .collect();
                tys.extend(fields.unnamed.iter().map(|f| f.ty.clone()));
                if container.untagged && binds.len() == 1 {
                    arms.push(quote!{Self::#vi(#(#binds),*)=>#krate::ToTerm::to_term(#(#binds),*,__ctx,__dest)})
                } else if container.untagged {
                    let writes=binds.iter().map(|b|quote!{let __term=__ctx.term()?;#krate::ToTerm::to_term(#b,__ctx,__term)?;__values.push(__term);});
                    arms.push(quote!{Self::#vi(#(#binds),*)=>{let mut __values=Vec::new();#(#writes)*#krate::codec::put_list_terms(__ctx,__dest,&__values)}})
                } else {
                    let writes=binds.iter().map(|b|quote!{let __term=__ctx.term()?;#krate::ToTerm::to_term(#b,__ctx,__term)?;__values.push(__term);});
                    arms.push(quote!{Self::#vi(#(#binds),*)=>{let mut __values=Vec::new();#(#writes)*#krate::codec::put_compound(__ctx,__dest,#wire,&__values)}})
                }
            }
            Fields::Named(fields) => {
                let binds: Vec<_> = fields
                    .named
                    .iter()
                    .map(|f| f.ident.clone().unwrap())
                    .collect();
                tys.extend(fields.named.iter().map(|f| f.ty.clone()));
                let writes=fields.named.iter().map(|f|{let n=f.ident.as_ref().unwrap();let w=attrs(&f.attrs).ok().and_then(|a|a.rename).unwrap_or_else(||rename(&n.to_string(),container.rename_all.as_deref()));quote!{let __term=__ctx.term()?;if #krate::ToTerm::__to_field(#n,__ctx,__term)?{__fields.push((#w.to_owned(),__term));}}});
                let tag = if container.untagged {
                    "#".to_owned()
                } else {
                    wire.clone()
                };
                arms.push(quote!{Self::#vi{#(#binds),*}=>{let mut __fields=Vec::new();#(#writes)*#krate::codec::put_dict(__ctx,__dest,#tag,__fields)}})
            }
        }
    }
    let generics = add_bound(input.generics.clone(), &tys, quote!(#krate::ToTerm));
    let (ig, tg, wc) = generics.split_for_impl();
    Ok(
        quote! {impl #ig #krate::ToTerm for #ident #tg #wc{fn to_term<__C:#krate::FliContext+?Sized>(&self,__ctx:&__C,__dest:#krate::Term<'_>)->Result<(),#krate::TermCodecError>{match self{#(#arms),*}}}},
    )
}

fn enum_from(
    input: &DeriveInput,
    data: &DataEnum,
    container: &Attrs,
    krate: &Path,
) -> syn::Result<proc_macro2::TokenStream> {
    if container.tag.is_some() {
        return tagged_enum_from(input, data, container, krate);
    }
    let ident = &input.ident;
    let mut tys = Vec::new();
    let mut attempts = Vec::new();
    for variant in &data.variants {
        let va = attrs(&variant.attrs)?;
        if va.skip || va.skip_from_term {
            continue;
        }
        let vi = &variant.ident;
        let wire = va
            .rename
            .unwrap_or_else(|| rename(&vi.to_string(), container.rename_all.as_deref()));
        match &variant.fields{
        Fields::Unit if container.untagged=>{},
        Fields::Unit=>attempts.push(quote!{if __term.kind()==#krate::TermKind::Atom&&__term.get_atom()?.text()==#wire{return Ok(Self::#vi);}}),
        Fields::Unnamed(fields)=>{let ftys:Vec<_>=fields.unnamed.iter().map(|f|f.ty.clone()).collect();tys.extend(ftys.clone());if container.untagged&&ftys.len()==1{let ty=&ftys[0];attempts.push(quote!{if let Ok(__v)=<#ty as #krate::FromTerm>::from_term(__ctx,__term){return Ok(Self::#vi(__v));}})}else if container.untagged{let arity=ftys.len();let vals=ftys.iter().enumerate().map(|(i,ty)|{let idx=syn::Index::from(i);quote!{<#ty as #krate::FromTerm>::from_term(__ctx,__args[#idx])?}});attempts.push(quote!{if let Ok(__value)=(||->Result<Self,#krate::TermCodecError>{let __args=__term.collect_list(__ctx)?;if __args.len()!=#arity{return Err(#krate::TermCodecError::ArityMismatch{expected:#arity,actual:__args.len()});}Ok(Self::#vi(#(#vals),*))})(){return Ok(__value);}})}else{let arity=ftys.len();let vals=ftys.iter().enumerate().map(|(i,ty)|{let idx=syn::Index::from(i);quote!{<#ty as #krate::FromTerm>::from_term(__ctx,__args[#idx])?}});attempts.push(quote!{if let Ok(__args)=#krate::codec::compound_args(__ctx,__term,#wire,#arity){return Ok(Self::#vi(#(#vals),*));}})}},
        Fields::Named(fields)=>{let mut reads=Vec::new();let mut names=Vec::new();for field in &fields.named{let n=field.ident.as_ref().unwrap();names.push(n);let ty=field.ty.clone();tys.push(ty.clone());let a=attrs(&field.attrs)?;let w=a.rename.unwrap_or_else(||rename(&n.to_string(),container.rename_all.as_deref()));reads.push(quote!{let #n:#ty=#krate::FromTerm::__from_field(__ctx,__fields.remove(#w),#w)?;});}let tag=if container.untagged{"#".to_owned()}else{wire.clone()};if container.untagged{attempts.push(quote!{if let Ok(__value)=(||->Result<Self,#krate::TermCodecError>{#krate::codec::require_dict_tag(__ctx,__term,#tag)?;let mut __fields=#krate::codec::dict_fields(__ctx,__term)?;#(#reads)*Ok(Self::#vi{#(#names),*})})(){return Ok(__value);}})}else{attempts.push(quote!{if #krate::codec::require_dict_tag(__ctx,__term,#tag).is_ok(){let mut __fields=#krate::codec::dict_fields(__ctx,__term)?;#(#reads)*return Ok(Self::#vi{#(#names),*});}})}}
    }
    }
    let generics = add_bound(input.generics.clone(), &tys, quote!(#krate::FromTerm));
    let (ig, tg, wc) = generics.split_for_impl();
    Ok(
        quote! {impl #ig #krate::FromTerm for #ident #tg #wc{fn from_term<__C:#krate::FliContext+?Sized>(__ctx:&__C,__term:#krate::Term<'_>)->Result<Self,#krate::TermCodecError>{#(#attempts)*let __variant=match __term.kind(){#krate::TermKind::Atom=>__term.get_atom()?.text(),#krate::TermKind::Compound=>__term.name_arity()?.0.text(),#krate::TermKind::Dict=>__term.dict_tag(__ctx)?.map(|a|a.text()).unwrap_or_default(),_=>String::new()};Err(#krate::TermCodecError::UnknownVariant{variant:__variant})}}},
    )
}

fn tagged_enum_to(
    input: &DeriveInput,
    data: &DataEnum,
    container: &Attrs,
    krate: &Path,
) -> syn::Result<proc_macro2::TokenStream> {
    let ident = &input.ident;
    let tag = container.tag.as_ref().unwrap();
    let content = container.content.as_deref();
    let mut tys = Vec::new();
    let mut arms = Vec::new();
    for variant in &data.variants {
        let va = attrs(&variant.attrs)?;
        if va.skip || va.skip_to_term {
            let vi = &variant.ident;
            let pattern = match &variant.fields {
                Fields::Unit => quote!(Self::#vi),
                Fields::Unnamed(_) => quote!(Self::#vi(..)),
                Fields::Named(_) => quote!(Self::#vi { .. }),
            };
            arms.push(quote! { #pattern => Err(#krate::TermCodecError::Message(
                concat!("variant ", stringify!(#vi), " is skipped for ToTerm").to_owned()
            )) });
            continue;
        }
        let vi = &variant.ident;
        let wire = va
            .rename
            .unwrap_or_else(|| rename(&vi.to_string(), container.rename_all.as_deref()));
        match &variant.fields{
        Fields::Unit=>arms.push(quote!{Self::#vi=>{let __tag=__ctx.term()?;__tag.put_string(#wire)?;#krate::codec::put_dict(__ctx,__dest,"#",vec![(#tag.to_owned(),__tag)])}}),
        Fields::Named(fields)=>{let binds:Vec<_>=fields.named.iter().map(|f|f.ident.clone().unwrap()).collect();tys.extend(fields.named.iter().map(|f|f.ty.clone()));let writes=fields.named.iter().map(|f|{let n=f.ident.as_ref().unwrap();let w=attrs(&f.attrs).ok().and_then(|a|a.rename).unwrap_or_else(||rename(&n.to_string(),container.rename_all.as_deref()));quote!{let __term=__ctx.term()?;if #krate::ToTerm::__to_field(#n,__ctx,__term)?{__fields.push((#w.to_owned(),__term));}}});if let Some(content)=content{arms.push(quote!{Self::#vi{#(#binds),*}=>{let __tag=__ctx.term()?;__tag.put_string(#wire)?;let mut __fields=Vec::new();#(#writes)*let __payload=__ctx.term()?;#krate::codec::put_dict(__ctx,__payload,"#",__fields)?;#krate::codec::put_dict(__ctx,__dest,"#",vec![(#tag.to_owned(),__tag),(#content.to_owned(),__payload)])}})}else{arms.push(quote!{Self::#vi{#(#binds),*}=>{let __tag=__ctx.term()?;__tag.put_string(#wire)?;let mut __fields=vec![(#tag.to_owned(),__tag)];#(#writes)*#krate::codec::put_dict(__ctx,__dest,"#",__fields)}})}},
        Fields::Unnamed(fields)=>{if content.is_none(){return Err(syn::Error::new_spanned(fields,"internally tagged tuple/newtype variants require content = ..."));}let binds:Vec<_>=(0..fields.unnamed.len()).map(|i|format_ident!("__v{i}")).collect();tys.extend(fields.unnamed.iter().map(|f|f.ty.clone()));let writes=binds.iter().map(|b|quote!{let __term=__ctx.term()?;#krate::ToTerm::to_term(#b,__ctx,__term)?;__values.push(__term);});let c=content.unwrap();arms.push(quote!{Self::#vi(#(#binds),*)=>{let __tag=__ctx.term()?;__tag.put_string(#wire)?;let mut __values=Vec::new();#(#writes)*let __payload=__ctx.term()?;if __values.len()==1{__payload.put_term(__values[0])?;}else{let mut __tail=__ctx.term()?;__tail.put_nil()?;for __item in __values.into_iter().rev(){let __cell=__ctx.term()?;__cell.cons_list(__item,__tail)?;__tail=__cell;}__payload.put_term(__tail)?;}#krate::codec::put_dict(__ctx,__dest,"#",vec![(#tag.to_owned(),__tag),(#c.to_owned(),__payload)])}})}
    }
    }
    let generics = add_bound(input.generics.clone(), &tys, quote!(#krate::ToTerm));
    let (ig, tg, wc) = generics.split_for_impl();
    Ok(
        quote! {impl #ig #krate::ToTerm for #ident #tg #wc{fn to_term<__C:#krate::FliContext+?Sized>(&self,__ctx:&__C,__dest:#krate::Term<'_>)->Result<(),#krate::TermCodecError>{match self{#(#arms),*}}}},
    )
}

fn tagged_enum_from(
    input: &DeriveInput,
    data: &DataEnum,
    container: &Attrs,
    krate: &Path,
) -> syn::Result<proc_macro2::TokenStream> {
    let ident = &input.ident;
    let tag = container.tag.as_ref().unwrap();
    let content = container.content.as_deref();
    let mut tys = Vec::new();
    let mut arms = Vec::new();
    for variant in &data.variants {
        let va = attrs(&variant.attrs)?;
        if va.skip || va.skip_from_term {
            continue;
        }
        let vi = &variant.ident;
        let wire = va
            .rename
            .unwrap_or_else(|| rename(&vi.to_string(), container.rename_all.as_deref()));
        match &variant.fields {
            Fields::Unit => arms.push(quote! {#wire=>Ok(Self::#vi)}),
            Fields::Named(fields) => {
                let mut reads = Vec::new();
                let mut names = Vec::new();
                for f in &fields.named {
                    let n = f.ident.as_ref().unwrap();
                    names.push(n);
                    let ty = f.ty.clone();
                    tys.push(ty.clone());
                    let w = attrs(&f.attrs)?
                        .rename
                        .unwrap_or_else(|| rename(&n.to_string(), container.rename_all.as_deref()));
                    reads.push(quote!{let #n:#ty=#krate::FromTerm::__from_field(__ctx,__payload.remove(#w),#w)?;});
                }
                let prep = if let Some(c) = content {
                    quote! {let __content=__fields.remove(#c).ok_or_else(||#krate::TermCodecError::MissingField{field:#c.to_owned()})?;let mut __payload=#krate::codec::dict_fields(__ctx,__content)?;}
                } else {
                    quote! {let mut __payload=__fields;}
                };
                arms.push(quote! {#wire=>{#prep #(#reads)*Ok(Self::#vi{#(#names),*})}})
            }
            Fields::Unnamed(fields) => {
                let c = content.ok_or_else(|| {
                    syn::Error::new_spanned(
                        fields,
                        "internally tagged tuple/newtype variants require content = ...",
                    )
                })?;
                let ftys: Vec<_> = fields.unnamed.iter().map(|f| f.ty.clone()).collect();
                tys.extend(ftys.clone());
                if ftys.len() == 1 {
                    let ty = &ftys[0];
                    arms.push(quote!{#wire=>{let __content=__fields.remove(#c).ok_or_else(||#krate::TermCodecError::MissingField{field:#c.to_owned()})?;Ok(Self::#vi(<#ty as #krate::FromTerm>::from_term(__ctx,__content)?))}})
                } else {
                    let len = ftys.len();
                    let vals = ftys.iter().enumerate().map(|(i, ty)| {
                        let idx = syn::Index::from(i);
                        quote! {<#ty as #krate::FromTerm>::from_term(__ctx,__items[#idx])?}
                    });
                    arms.push(quote!{#wire=>{let __content=__fields.remove(#c).ok_or_else(||#krate::TermCodecError::MissingField{field:#c.to_owned()})?;let __items=__content.collect_list(__ctx)?;if __items.len()!=#len{return Err(#krate::TermCodecError::ArityMismatch{expected:#len,actual:__items.len()});}Ok(Self::#vi(#(#vals),*))}})
                }
            }
        }
    }
    let generics = add_bound(input.generics.clone(), &tys, quote!(#krate::FromTerm));
    let (ig, tg, wc) = generics.split_for_impl();
    Ok(
        quote! {impl #ig #krate::FromTerm for #ident #tg #wc{fn from_term<__C:#krate::FliContext+?Sized>(__ctx:&__C,__term:#krate::Term<'_>)->Result<Self,#krate::TermCodecError>{let mut __fields=#krate::codec::dict_fields(__ctx,__term)?;let __tag=__fields.remove(#tag).ok_or_else(||#krate::TermCodecError::MissingField{field:#tag.to_owned()})?.get_text()?;match __tag.as_str(){#(#arms),*,_=>Err(#krate::TermCodecError::UnknownVariant{variant:__tag})}}}},
    )
}
