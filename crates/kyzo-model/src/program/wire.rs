/*
 * Copyright 2026, The KyzoDB Authors.
 *
 * This Source Code Form is subject to the terms of the Mozilla Public License, v. 2.0.
 * If a copy of the MPL was not distributed with this file,
 * You can obtain one at https://mozilla.org/MPL/2.0/.
 */

//! Seat-59 program-IR wire door — ONE authority for "persist fields, mint
//! empty span/trivia on decode".
//!
//! Hand-rolled `Serialize`/`Deserialize` twins across query/rule atoms were
//! a second authority by copy-paste (copy_detector). Call sites declare
//! wire fields only; they do not re-own the omit-span law.

/// Persist the listed fields; decode mints [`crate::SourceSpan::empty`].
/// Optional `+ trivia` also mints [`super::rule::Trivia::empty`].
macro_rules! wire_omit_span {
    ($ty:ident { $($field:ident : $t:ty),+ $(,)? }) => {
        impl ::serde::Serialize for $ty {
            fn serialize<__S: ::serde::Serializer>(
                &self,
                serializer: __S,
            ) -> ::std::result::Result<__S::Ok, __S::Error> {
                #[derive(::serde_derive::Serialize)]
                struct __Wire<'__a> {
                    $($field: &'__a $t,)+
                }
                __Wire {
                    $($field: &self.$field,)+
                }
                .serialize(serializer)
            }
        }

        impl<'__de> ::serde::Deserialize<'__de> for $ty {
            fn deserialize<__D: ::serde::Deserializer<'__de>>(
                deserializer: __D,
            ) -> ::std::result::Result<Self, __D::Error> {
                #[derive(::serde_derive::Deserialize)]
                struct __Wire {
                    $($field: $t,)+
                }
                let w = <__Wire as ::serde::Deserialize>::deserialize(deserializer)?;
                ::std::result::Result::Ok($ty {
                    $($field: w.$field,)+
                    span: $crate::SourceSpan::empty(),
                })
            }
        }
    };
    ($ty:ident { $($field:ident : $t:ty),+ $(,)? } + trivia) => {
        impl ::serde::Serialize for $ty {
            fn serialize<__S: ::serde::Serializer>(
                &self,
                serializer: __S,
            ) -> ::std::result::Result<__S::Ok, __S::Error> {
                #[derive(::serde_derive::Serialize)]
                struct __Wire<'__a> {
                    $($field: &'__a $t,)+
                }
                __Wire {
                    $($field: &self.$field,)+
                }
                .serialize(serializer)
            }
        }

        impl<'__de> ::serde::Deserialize<'__de> for $ty {
            fn deserialize<__D: ::serde::Deserializer<'__de>>(
                deserializer: __D,
            ) -> ::std::result::Result<Self, __D::Error> {
                #[derive(::serde_derive::Deserialize)]
                struct __Wire {
                    $($field: $t,)+
                }
                let w = <__Wire as ::serde::Deserialize>::deserialize(deserializer)?;
                ::std::result::Result::Ok($ty {
                    $($field: w.$field,)+
                    span: $crate::SourceSpan::empty(),
                    trivia: $crate::program::rule::Trivia::empty(),
                })
            }
        }
    };
}

pub(crate) use wire_omit_span;
