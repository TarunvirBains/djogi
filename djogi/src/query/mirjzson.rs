//! Trusted-provenance JSON predicate builders over Djogi `MirJzSON` fields.
//! # Why a Djogi wrapper over Sassi's `JSahibONFieldRef`?
//! Sassi's `Field<T, JSahibON>::jsahibon` returns a `JSahibONFieldRef<T>`
//! that builds `BasicPredicate<T>` leaves carrying [`sassi::LookupOp::Json`].
//! Those raw Sassi builders accept any `&'static str` for the column name
//! (Sassi has no Djogi-side identifier validator) and any closure for the
//! extractor. The same forgery threats `PortablePredicate<T>` already
//! guards against for non-JSON predicates apply here in spades:
//! 1. **Field-name forgery** — a downstream caller (or a hostile macro
//!    expansion) could construct `sassi::Field::new("payload", |m|
//! &m.unrelated_jsahibon_field)` and produce a `BasicPredicate` whose
//!    column name targets the database `payload` column while Punnu-side
//!    evaluation reads `m.unrelated_jsahibon_field`. The two sides drift.
//! 2. **Identifier smuggling** — Sassi accepts any `&'static str`. Djogi's
//!    SQL emitter routes through `SqlAccumulator::push_sql`, which
//!    assumes its inputs were already validated against the plain-
//!    identifier gate Djogi applies to every other root column.
//! 3. **Path / key smuggling** — JSON paths in Sassi's contract are
//!    sequences of literal UTF-8 strings. Djogi's SQL emission **binds**
//!    those strings as parameters (not interpolates them); the
//!    Djogi-vs-Sassi contract is that the path string the wrapper
//!    captured is the exact string the SQL emitter binds.
//!    The Djogi wrappers below close every threat by:
//! - Refusing to accept a caller-supplied `field_name` — the column name
//!   is captured from the trusted Djogi-private `__sql_field` route on
//!   [`DjogiField<M, MirJzSON>`](crate::query::field::DjogiField) at
//!   construction time, which routes through Djogi's own
//!   identifier-validation gate.
//! - Stamping each emitted `PortablePredicate` with a
//!   [`DjogiFieldProvenance`] token. The SQL lowering route checks the
//!   provenance at the `LookupOp::Json` arm and refuses to lower a leaf
//!   that lacks it (closes the "downstream code imports Sassi
//!   directly and hands Djogi a forged `LookupOp::Json` predicate"
//!   threat).
//! - Routing every Punnu-side closure through Sassi's own
//!   `evaluate_jsahibon_predicate` — the Djogi wrappers never reimplement
//!   the truth rules.
//! # How the extractor lift works
//! Sassi's `Field<T, V>::new` requires a `fn(&T) -> &V` pointer (not a
//! closure — closures cannot be coerced to function pointers without
//! capturing nothing). The Djogi macro stamps each `MirJzSON` column as
//! a `fn(&M) -> &MirJzSON`. To plug into Sassi's
//! `Field<M, sassi::JSahibON>::jsahibon` builder we need a
//! `fn(&M) -> &sassi::JSahibON`.
//! The lift relies on the `#[repr(transparent)]` annotation on
//! [`MirJzSON`]: under that annotation, `&MirJzSON` has identical
//! layout and ABI to `&sassi::JSahibON`, and `Option<MirJzSON>` has
//! identical layout to `Option<sassi::JSahibON>` (niche optimisations
//! survive `repr(transparent)` per the Rust reference). The
//! `std::mem::transmute` calls below are sound for that exact reason
//! and only that reason — adding any other field to [`MirJzSON`] (even
//! a zero-sized one) would break the invariant and must trip the
//! `repr(transparent)` lint.

use crate::jsonb::MirJzSON;
use crate::model::Model;
use crate::query::field::{DjogiField, DjogiFieldProvenance};
use crate::query::predicate::PortablePredicate;
use sassi::predicate::{
    JSahibONFieldRef, JSahibONOptionFieldRef, JSahibONPathRef, JSahibONValueRef, JTypeKind,
};
use sassi::{BasicPredicate, JOrderedScalar, JSahibON, JScalar};
use std::marker::PhantomData;

/// Trusted-provenance JSON predicate builder for a `MirJzSON` column.
/// Produced by [`DjogiField<M, MirJzSON>::jsahibon`]. Every predicate
/// method returns a Djogi [`PortablePredicate<M>`] carrying a
/// [`DjogiFieldProvenance`] token — SQL lowering accepts
/// `LookupOp::Json` leaves only through this trusted path.
pub struct DjogiJSahibONFieldRef<M: Model> {
    /// The Sassi-side builder. All predicate construction routes through
    /// this so Djogi never reimplements Sassi's truth rules.
    inner: JSahibONFieldRef<M>,
}

/// Trusted-provenance JSON predicate builder for an
/// `Option<MirJzSON>` column.
/// Produced by [`DjogiField<M, Option<MirJzSON>>::jsahibon`]. Mirrors
/// [`DjogiJSahibONFieldRef`] but `exists` / `missing` distinguish
/// `None` (missing) from `Some(MirJzSON(JSahibON::Null))` (present,
/// JSON `null`).
pub struct DjogiJSahibONOptionFieldRef<M: Model> {
    inner: JSahibONOptionFieldRef<M>,
}

/// Trusted-provenance JSON predicate builder anchored at a specific
/// JSON path within a [`MirJzSON`] field.
/// Produced by the `path` / `key` / `path_segments` methods on
/// [`DjogiJSahibONFieldRef`] / [`DjogiJSahibONOptionFieldRef`]. Carries
/// the predicate-construction surface for the resolved value at that
/// path.
pub struct DjogiJSahibONPathRef<M: Model> {
    inner: JSahibONPathRef<M>,
}

/// Trusted-provenance scalar comparison builder produced by
/// [`DjogiJSahibONPathRef::value`] / [`DjogiJSahibONFieldRef::value`] /
/// [`DjogiJSahibONOptionFieldRef::value`].
/// `V` must implement [`JScalar`] (and [`JOrderedScalar`] for ordering
/// methods).
pub struct DjogiJSahibONValueRef<M: Model, V> {
    inner: JSahibONValueRef<M, V>,
    _marker: PhantomData<fn(&M, V)>,
}

// ── Clone impls — manual, mirror Sassi's manual Clone on the
// underlying builders (avoiding spurious `M: Clone` bounds the derive
// would impose). ──────────────────────────────────────────────────────────

impl<M: Model> Clone for DjogiJSahibONFieldRef<M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<M: Model> Clone for DjogiJSahibONOptionFieldRef<M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<M: Model> Clone for DjogiJSahibONPathRef<M> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<M: Model, V> Clone for DjogiJSahibONValueRef<M, V> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
            _marker: PhantomData,
        }
    }
}

// ── Methods — mirror Sassi's surface and stamp Djogi provenance on
// every returned predicate. Path walks return new path-anchored builders
// (no provenance there; provenance is stamped only on terminal predicate
// values). ────────────────────────────────────────────────────────────────

macro_rules! impl_delegated_methods {
    ($ty:ident) => {
        impl<M: Model> $ty<M> {
            /// Return a path ref anchored at the JSON field root.
            /// See [`JSahibONFieldRef::root`].
            pub fn root(&self) -> DjogiJSahibONPathRef<M> {
                DjogiJSahibONPathRef {
                    inner: self.inner.root(),
                }
            }

            /// Return a path ref for a dotted plain-identifier path.
            /// See [`JSahibONFieldRef::path`]. The dotted-identifier
            /// grammar matches Djogi's existing `JsonbPathRef::path`:
            /// each segment must be a non-empty ASCII identifier
            /// (starting with an ASCII letter or `_`, continuing with
            /// ASCII alphanumerics or `_`) of at most 63 bytes.
            /// Use [`key`](Self::key) or
            /// [`path_segments`](Self::path_segments) for arbitrary keys.
            /// # Panics
            /// Panics on a segment that fails the plain-identifier check
            /// the function is intended for `'static` literals authored
            /// at compile time.
            pub fn path(&self, dotted_plain_idents: &'static str) -> DjogiJSahibONPathRef<M> {
                DjogiJSahibONPathRef {
                    inner: self.inner.path(dotted_plain_idents),
                }
            }

            /// Return a path ref for a literal object key below the root.
            pub fn key(&self, key: impl Into<String>) -> DjogiJSahibONPathRef<M> {
                DjogiJSahibONPathRef {
                    inner: self.inner.key(key),
                }
            }

            /// Return a path ref from literal object-key segments.
            pub fn path_segments<I, S>(&self, segments: I) -> DjogiJSahibONPathRef<M>
            where
                I: IntoIterator<Item = S>,
                S: Into<String>,
            {
                DjogiJSahibONPathRef {
                    inner: self.inner.path_segments(segments),
                }
            }

            /// Predicate: path resolves to any JSON value.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn exists(&self) -> PortablePredicate<M> {
                wrap_predicate(self.inner.exists())
            }

            /// Predicate: path does not resolve.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn missing(&self) -> PortablePredicate<M> {
                wrap_predicate(self.inner.missing())
            }

            /// Predicate: path resolves to JSON `null`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn is_json_null(&self) -> PortablePredicate<M> {
                wrap_predicate(self.inner.is_json_null())
            }

            /// Predicate: path resolves to a non-null JSON value.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn is_not_json_null(&self) -> PortablePredicate<M> {
                wrap_predicate(self.inner.is_not_json_null())
            }

            /// Predicate: resolved value matches `kind`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn is_type(&self, kind: JTypeKind) -> PortablePredicate<M> {
                wrap_predicate(self.inner.is_type(kind))
            }

            /// Shorthand for `is_type(JTypeKind::Bool)`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn is_bool(&self) -> PortablePredicate<M> {
                wrap_predicate(self.inner.is_bool())
            }

            /// Shorthand for `is_type(JTypeKind::Number)`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn is_number(&self) -> PortablePredicate<M> {
                wrap_predicate(self.inner.is_number())
            }

            /// Shorthand for `is_type(JTypeKind::String)`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn is_string(&self) -> PortablePredicate<M> {
                wrap_predicate(self.inner.is_string())
            }

            /// Shorthand for `is_type(JTypeKind::Array)`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn is_array(&self) -> PortablePredicate<M> {
                wrap_predicate(self.inner.is_array())
            }

            /// Shorthand for `is_type(JTypeKind::Object)`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn is_object(&self) -> PortablePredicate<M> {
                wrap_predicate(self.inner.is_object())
            }

            /// Predicate: resolved object contains `key`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn has_key(&self, key: impl Into<String>) -> PortablePredicate<M> {
                wrap_predicate(self.inner.has_key(key))
            }

            /// Predicate: resolved object contains at least one of `keys`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn has_any_key<I, S>(&self, keys: I) -> PortablePredicate<M>
            where
                I: IntoIterator<Item = S>,
                S: Into<String>,
            {
                wrap_predicate(self.inner.has_any_key(keys))
            }

            /// Predicate: resolved object contains every key in `keys`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn has_all_keys<I, S>(&self, keys: I) -> PortablePredicate<M>
            where
                I: IntoIterator<Item = S>,
                S: Into<String>,
            {
                wrap_predicate(self.inner.has_all_keys(keys))
            }

            /// Begin a typed scalar comparison against the resolved value.
            pub fn value<V: JScalar>(&self) -> DjogiJSahibONValueRef<M, V> {
                DjogiJSahibONValueRef {
                    inner: self.inner.value(),
                    _marker: PhantomData,
                }
            }

            /// Predicate: resolved JSON value equals `value`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn eq_json(&self, value: JSahibON) -> PortablePredicate<M> {
                wrap_predicate(self.inner.eq_json(value))
            }

            /// Predicate: resolved JSON value differs from `value`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn neq_json(&self, value: JSahibON) -> PortablePredicate<M> {
                wrap_predicate(self.inner.neq_json(value))
            }

            /// Predicate: resolved array contains `element`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn array_contains(&self, element: JSahibON) -> PortablePredicate<M> {
                wrap_predicate(self.inner.array_contains(element))
            }

            /// Predicate: resolved array length equals `len`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn array_len_eq(&self, len: usize) -> PortablePredicate<M> {
                wrap_predicate(self.inner.array_len_eq(len))
            }

            /// Predicate: resolved array length is greater than `len`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn array_len_gt(&self, len: usize) -> PortablePredicate<M> {
                wrap_predicate(self.inner.array_len_gt(len))
            }

            /// Predicate: resolved array length is at least `len`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn array_len_gte(&self, len: usize) -> PortablePredicate<M> {
                wrap_predicate(self.inner.array_len_gte(len))
            }

            /// Predicate: resolved array length is less than `len`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn array_len_lt(&self, len: usize) -> PortablePredicate<M> {
                wrap_predicate(self.inner.array_len_lt(len))
            }

            /// Predicate: resolved array length is at most `len`.
            #[must_use = "predicates are lazy — dropping one silently omits the filter"]
            pub fn array_len_lte(&self, len: usize) -> PortablePredicate<M> {
                wrap_predicate(self.inner.array_len_lte(len))
            }
        }
    };
}

impl_delegated_methods!(DjogiJSahibONFieldRef);
impl_delegated_methods!(DjogiJSahibONOptionFieldRef);

// ── Path-anchored builder ─────────────────────────────────────────────────

impl<M: Model> DjogiJSahibONPathRef<M> {
    /// Push an additional literal object key onto this path.
    pub fn key(self, key: impl Into<String>) -> Self {
        Self {
            inner: self.inner.key(key),
        }
    }

    /// Append additional literal segments onto this path.
    pub fn path_segments<I, S>(self, segments: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            inner: self.inner.path_segments(segments),
        }
    }

    /// Predicate: path resolves to any JSON value.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn exists(&self) -> PortablePredicate<M> {
        wrap_predicate(self.inner.exists())
    }

    /// Predicate: path does not resolve.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn missing(&self) -> PortablePredicate<M> {
        wrap_predicate(self.inner.missing())
    }

    /// Predicate: path resolves to JSON `null`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn is_json_null(&self) -> PortablePredicate<M> {
        wrap_predicate(self.inner.is_json_null())
    }

    /// Predicate: path resolves to a non-null JSON value.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn is_not_json_null(&self) -> PortablePredicate<M> {
        wrap_predicate(self.inner.is_not_json_null())
    }

    /// Predicate: resolved value matches `kind`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn is_type(&self, kind: JTypeKind) -> PortablePredicate<M> {
        wrap_predicate(self.inner.is_type(kind))
    }

    /// Shorthand for `is_type(JTypeKind::Bool)`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn is_bool(&self) -> PortablePredicate<M> {
        wrap_predicate(self.inner.is_bool())
    }

    /// Shorthand for `is_type(JTypeKind::Number)`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn is_number(&self) -> PortablePredicate<M> {
        wrap_predicate(self.inner.is_number())
    }

    /// Shorthand for `is_type(JTypeKind::String)`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn is_string(&self) -> PortablePredicate<M> {
        wrap_predicate(self.inner.is_string())
    }

    /// Shorthand for `is_type(JTypeKind::Array)`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn is_array(&self) -> PortablePredicate<M> {
        wrap_predicate(self.inner.is_array())
    }

    /// Shorthand for `is_type(JTypeKind::Object)`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn is_object(&self) -> PortablePredicate<M> {
        wrap_predicate(self.inner.is_object())
    }

    /// Predicate: resolved object contains `key`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn has_key(&self, key: impl Into<String>) -> PortablePredicate<M> {
        wrap_predicate(self.inner.has_key(key))
    }

    /// Predicate: resolved object contains at least one of `keys`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn has_any_key<I, S>(&self, keys: I) -> PortablePredicate<M>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        wrap_predicate(self.inner.has_any_key(keys))
    }

    /// Predicate: resolved object contains every key in `keys`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn has_all_keys<I, S>(&self, keys: I) -> PortablePredicate<M>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        wrap_predicate(self.inner.has_all_keys(keys))
    }

    /// Begin a typed scalar comparison against the resolved value.
    pub fn value<V: JScalar>(&self) -> DjogiJSahibONValueRef<M, V> {
        DjogiJSahibONValueRef {
            inner: self.inner.value(),
            _marker: PhantomData,
        }
    }

    /// Predicate: resolved JSON value equals `value`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn eq_json(&self, value: JSahibON) -> PortablePredicate<M> {
        wrap_predicate(self.inner.eq_json(value))
    }

    /// Predicate: resolved JSON value differs from `value`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn neq_json(&self, value: JSahibON) -> PortablePredicate<M> {
        wrap_predicate(self.inner.neq_json(value))
    }

    /// Predicate: resolved array contains `element`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn array_contains(&self, element: JSahibON) -> PortablePredicate<M> {
        wrap_predicate(self.inner.array_contains(element))
    }

    /// Predicate: resolved array length equals `len`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn array_len_eq(&self, len: usize) -> PortablePredicate<M> {
        wrap_predicate(self.inner.array_len_eq(len))
    }

    /// Predicate: resolved array length is greater than `len`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn array_len_gt(&self, len: usize) -> PortablePredicate<M> {
        wrap_predicate(self.inner.array_len_gt(len))
    }

    /// Predicate: resolved array length is at least `len`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn array_len_gte(&self, len: usize) -> PortablePredicate<M> {
        wrap_predicate(self.inner.array_len_gte(len))
    }

    /// Predicate: resolved array length is less than `len`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn array_len_lt(&self, len: usize) -> PortablePredicate<M> {
        wrap_predicate(self.inner.array_len_lt(len))
    }

    /// Predicate: resolved array length is at most `len`.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn array_len_lte(&self, len: usize) -> PortablePredicate<M> {
        wrap_predicate(self.inner.array_len_lte(len))
    }
}

// ── Typed scalar comparison builder ───────────────────────────────────────

impl<M: Model, V: JScalar> DjogiJSahibONValueRef<M, V> {
    /// Predicate: `value == operand`.
    /// # Panics
    /// When `V = f64`, panics if `value` is non-finite (Sassi rejects
    /// `NaN` / `±Infinity` in its `JFiniteF64` carrier).
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn eq(&self, value: V) -> PortablePredicate<M> {
        wrap_predicate(self.inner.eq(value))
    }

    /// Predicate: `value != operand`.
    /// # Panics
    /// When `V = f64`, panics if `value` is non-finite.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn neq(&self, value: V) -> PortablePredicate<M> {
        wrap_predicate(self.inner.neq(value))
    }

    /// Predicate: `value IN (values...)`.
    /// # Panics
    /// When `V = f64`, panics if any element of `values` is non-finite.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn in_(&self, values: Vec<V>) -> PortablePredicate<M> {
        wrap_predicate(self.inner.in_(values))
    }

    /// Predicate: `value NOT IN (values...)`.
    /// # Panics
    /// When `V = f64`, panics if any element of `values` is non-finite.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn not_in(&self, values: Vec<V>) -> PortablePredicate<M> {
        wrap_predicate(self.inner.not_in(values))
    }
}

impl<M: Model, V: JOrderedScalar> DjogiJSahibONValueRef<M, V> {
    /// Predicate: `value > operand`.
    /// # Panics
    /// When `V = f64`, panics if `value` is non-finite.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn gt(&self, value: V) -> PortablePredicate<M> {
        wrap_predicate(self.inner.gt(value))
    }

    /// Predicate: `value >= operand`.
    /// # Panics
    /// When `V = f64`, panics if `value` is non-finite.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn gte(&self, value: V) -> PortablePredicate<M> {
        wrap_predicate(self.inner.gte(value))
    }

    /// Predicate: `value < operand`.
    /// # Panics
    /// When `V = f64`, panics if `value` is non-finite.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn lt(&self, value: V) -> PortablePredicate<M> {
        wrap_predicate(self.inner.lt(value))
    }

    /// Predicate: `value <= operand`.
    /// # Panics
    /// When `V = f64`, panics if `value` is non-finite.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn lte(&self, value: V) -> PortablePredicate<M> {
        wrap_predicate(self.inner.lte(value))
    }

    /// Predicate: `low <= value <= high` (inclusive on both ends).
    /// # Panics
    /// When `V = f64`, panics if either `low` or `high` is non-finite.
    #[must_use = "predicates are lazy — dropping one silently omits the filter"]
    pub fn between(&self, low: V, high: V) -> PortablePredicate<M> {
        wrap_predicate(self.inner.between(low, high))
    }
}

/// Stamp a Sassi `BasicPredicate<M>` with Djogi trusted-provenance.
/// Every predicate construction routes through here, so the
/// [`DjogiFieldProvenance`] mint call lives in exactly one place. The
/// `DjogiField<M, MirJzSON>::jsahibon` accessor is the only public
/// entry point that can reach this helper (transitively, through the
/// typed wrappers above) — adopter code cannot stamp arbitrary Sassi
/// predicates with trusted provenance.
fn wrap_predicate<M: Model>(bp: BasicPredicate<M>) -> PortablePredicate<M> {
    PortablePredicate::from_djogi_field(bp, DjogiFieldProvenance::__mirjzson_mint())
}

// ── DjogiField root-field accessors ───────────────────────────────────────
// Adds `.jsahibon` on the public `DjogiField<M, MirJzSON>` and
// `DjogiField<M, Option<MirJzSON>>` surfaces. The body constructs a
// Sassi `Field<M, JSahibON>` / `Field<M, Option<JSahibON>>` by
// transmuting the Djogi-trusted extractor through the
// `#[repr(transparent)]` layout-equivalence of `MirJzSON` and
// `sassi::JSahibON`, then calls `jsahibon` on the resulting Sassi
// builder to enter the predicate-construction surface.

impl<M: Model> DjogiField<M, MirJzSON> {
    /// Enter the trusted-provenance JSON predicate builder.
    /// Returns a [`DjogiJSahibONFieldRef<M>`] whose predicate methods
    /// produce trusted [`PortablePredicate<M>`] values. SQL lowering
    /// accepts `LookupOp::Json` leaves only through this trusted path
    /// raw `sassi::Field::new(...).jsahibon` predicates are rejected
    /// with [`PortablePredicateError::UntrustedJsonPredicate`].
    /// # Why this method?
    /// [`MirJzSON`] deliberately does **not** implement `PartialEq` /
    /// `Eq` / `Hash` / `PartialOrd`, so the root [`DjogiField::eq`]
    /// surface does not compile for `MirJzSON` columns. Whole-document
    /// JSON equality goes through
    /// [`DjogiJSahibONFieldRef::eq_json`](DjogiJSahibONFieldRef::eq_json)
    /// (object equality is order-insensitive; numeric carriers are
    /// softened across `I64` / `U64` / `F64`).
    /// # Example
    /// ```ignore
    /// use djogi::prelude::*;
    ///
    /// Post::objects().filter(|f| {
    ///     f.payload()
    ///         .jsahibon()
    ///         .path("engine.cylinders")
    ///         .value::<i64>()
    ///         .gte(4)
    /// });
    /// ```
    /// [`PortablePredicateError::UntrustedJsonPredicate`]:
    /// crate::query::PortablePredicateError::UntrustedJsonPredicate
    #[must_use = "the JSON builder is lazy — call a predicate method to produce a filter"]
    pub fn jsahibon(self) -> DjogiJSahibONFieldRef<M> {
        let column = self.__sql_field().column();
        // Pull the extractor off the Djogi wrapper. `DjogiField::extractor`
        // is `pub(crate)`, and `__make_djogi_field` stamps it from the
        // same `fn(&M) -> &MirJzSON` pointer the macro emits.
        let extract_mirjzson: fn(&M) -> &MirJzSON = self.extractor;
        // SAFETY: `MirJzSON` is `#[repr(transparent)]` over `sassi::JSahibON`.
        // The Rust reference guarantees identical layout, ABI, and reference
        // representation, so `fn(&M) -> &MirJzSON` and
        // `fn(&M) -> &sassi::JSahibON` are ABI-compatible. Adding any other
        // field to `MirJzSON` (even a zero-sized one) would invalidate the
        // `repr(transparent)` invariant and the transmute would no longer
        // be sound — the layout annotation on `MirJzSON` carries an explicit
        // "load-bearing for the query path" doc.
        let extract_jsahibon: fn(&M) -> &JSahibON =
            unsafe { std::mem::transmute(extract_mirjzson) };
        let field = sassi::Field::<M, JSahibON>::new(column, extract_jsahibon);
        DjogiJSahibONFieldRef {
            inner: field.jsahibon(),
        }
    }
}

impl<M: Model> DjogiField<M, Option<MirJzSON>> {
    /// Enter the trusted-provenance JSON predicate builder for an
    /// optional `MirJzSON` column.
    /// Returns a [`DjogiJSahibONOptionFieldRef<M>`]. `exists` is true
    /// only for `Some(_)`; `missing` is true only for `None`.
    /// `Some(MirJzSON(JSahibON::Null))` exists and is JSON `null`.
    /// See [`DjogiField<M, MirJzSON>::jsahibon`] for the design
    /// rationale.
    #[must_use = "the JSON builder is lazy — call a predicate method to produce a filter"]
    pub fn jsahibon(self) -> DjogiJSahibONOptionFieldRef<M> {
        let column = self.__sql_field().column();
        let extract_opt_mirjzson: fn(&M) -> &Option<MirJzSON> = self.extractor;
        // SAFETY: `MirJzSON` is `#[repr(transparent)]` over `sassi::JSahibON`.
        // Niche optimisations on `Option<T>` survive `repr(transparent)` per
        // the Rust reference: `Option<MirJzSON>` has the same layout as
        // `Option<sassi::JSahibON>`, so the reference and function-pointer
        // transmute below is sound. See the safety doc on
        // `DjogiField<M, MirJzSON>::jsahibon` for the broader invariant.
        let extract_opt_jsahibon: fn(&M) -> &Option<JSahibON> =
            unsafe { std::mem::transmute(extract_opt_mirjzson) };
        let field = sassi::Field::<M, Option<JSahibON>>::new(column, extract_opt_jsahibon);
        DjogiJSahibONOptionFieldRef {
            inner: field.jsahibon(),
        }
    }
}

// ── ExplicitPgPredicateField — SQL-only route stub ────────────────────────
// Per the spec: `.explicit_pg_predicate.mirjzson` is reserved for
// future PostgreSQL-only operators (`@?` / `@@`, GIN-specific shapes).
// V1 exposes the entry point so the API shape is committed, but the
// returned wrapper carries NO v1 portable-shape predicate methods
// every JSON query in v1 flows through `.jsahibon`.

/// PostgreSQL-only predicate view of a `MirJzSON` column.
/// Produced by `ExplicitPgPredicateField::<M, MirJzSON>::mirjzson`
/// the `mirjzson` impl block on
/// [`ExplicitPgPredicateField`](crate::query::field::ExplicitPgPredicateField)
/// for `V = MirJzSON` (and the `Option<MirJzSON>` sibling). **V1
/// exposes no predicate methods on this type** — every JSON query goes
/// through [`DjogiField<M, MirJzSON>::jsahibon`] so it is both
/// SQL-lowerable and Punnu-evaluable.
/// The entry point is reserved for future PostgreSQL-specific operators
/// (`@?` / `@@` JSONPath, GIN-specific shapes) that have no Sassi-local
/// contract. Future methods will emit `Condition::MirJzSON(_)` (or a
/// successor variant) and will be rejected by the cache/refresh
/// portability gate — adopters reaching for them are explicitly opting
/// out of cache portability for the SQL-only behaviour.
pub struct MirJzSONFieldRef<M: Model> {
    _column: &'static str,
    _marker: PhantomData<fn() -> M>,
}

impl<M: Model> std::fmt::Debug for MirJzSONFieldRef<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirJzSONFieldRef")
            .field("column", &self._column)
            .finish_non_exhaustive()
    }
}

/// PostgreSQL-only predicate view of an `Option<MirJzSON>` column.
/// Mirror of [`MirJzSONFieldRef`] for the optional case. See that
/// type's docs for the v1 contract — no portable-shape predicate
/// methods in v1.
pub struct MirJzSONOptionFieldRef<M: Model> {
    _column: &'static str,
    _marker: PhantomData<fn() -> M>,
}

impl<M: Model> std::fmt::Debug for MirJzSONOptionFieldRef<M> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MirJzSONOptionFieldRef")
            .field("column", &self._column)
            .finish_non_exhaustive()
    }
}

impl<M: Model> crate::query::field::ExplicitPgPredicateField<M, MirJzSON> {
    /// Enter the PostgreSQL-only `MirJzSON` predicate surface.
    /// **V1 exposes no predicate methods on the returned type.** Every
    /// JSON predicate in v1 flows through
    /// [`DjogiField<M, MirJzSON>::jsahibon`] so it is both SQL-lowerable
    /// and Punnu-evaluable. The entry point is reserved so future
    /// PostgreSQL-only operators (`@?` / `@@` JSONPath, GIN-specific
    /// shapes) can land without reshaping the API.
    /// If you reached for this method expecting v1 predicate methods,
    /// route through `.jsahibon` instead — that is the v1 contract.
    #[must_use = "the PG predicate view is lazy — call a method to produce a filter"]
    pub fn mirjzson(self) -> MirJzSONFieldRef<M> {
        MirJzSONFieldRef {
            _column: self.__column(),
            _marker: PhantomData,
        }
    }
}

impl<M: Model> crate::query::field::ExplicitPgPredicateField<M, Option<MirJzSON>> {
    /// Enter the PostgreSQL-only `Option<MirJzSON>` predicate surface.
    /// See the `mirjzson` method on the
    /// [`ExplicitPgPredicateField`](crate::query::field::ExplicitPgPredicateField)
    /// impl block for `V = MirJzSON` (the required-field sibling) for
    /// the v1 contract — no portable-shape predicate methods in v1.
    #[must_use = "the PG predicate view is lazy — call a method to produce a filter"]
    pub fn mirjzson(self) -> MirJzSONOptionFieldRef<M> {
        MirJzSONOptionFieldRef {
            _column: self.__column(),
            _marker: PhantomData,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::DjogiError;
    use crate::descriptor::ModelDescriptor;
    use std::future::Future;

    /// Minimal inert `Model` with a single `MirJzSON` payload field.
    /// Mirrors the test fixtures in `query::predicate` so the new builder
    /// path can exercise predicate construction without a live database.
    #[derive(Debug)]
    struct Fake {
        payload: MirJzSON,
        maybe: Option<MirJzSON>,
    }

    impl crate::model::__sealed::Sealed for Fake {}
    #[allow(clippy::manual_async_fn)]
    impl Model for Fake {
        type Pk = i64;
        type Fields = ();
        fn table_name() -> &'static str {
            "fakes"
        }
        fn pk_value(&self) -> &i64 {
            unimplemented!()
        }
        fn descriptor() -> &'static ModelDescriptor {
            unimplemented!()
        }
        fn get(
            _ctx: &mut crate::context::DjogiContext,
            _id: i64,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn create(
            _ctx: &mut crate::context::DjogiContext,
            _v: Self,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn save<'ctx>(
            &'ctx mut self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<(), DjogiError>> + Send + 'ctx {
            async { unimplemented!() }
        }
        fn delete(
            self,
            _ctx: &mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<(), DjogiError>> + Send {
            async { unimplemented!() }
        }
        fn refresh_from_db<'ctx>(
            &'ctx self,
            _ctx: &'ctx mut crate::context::DjogiContext,
        ) -> impl Future<Output = Result<Self, DjogiError>> + Send + 'ctx {
            async { unimplemented!() }
        }
    }

    fn payload_field() -> DjogiField<Fake, MirJzSON> {
        crate::query::field::djogi_field_macro_support::__make_djogi_field::<Fake, MirJzSON>(
            "payload",
            |f| &f.payload,
        )
    }

    fn maybe_field() -> DjogiField<Fake, Option<MirJzSON>> {
        crate::query::field::djogi_field_macro_support::__make_djogi_field::<Fake, Option<MirJzSON>>(
            "maybe",
            |f| &f.maybe,
        )
    }

    /// `.jsahibon.exists` builds a `PortablePredicate<M>` carrying
    /// Djogi trusted provenance.
    #[test]
    fn exists_stamps_djogi_provenance() {
        let predicate = payload_field().jsahibon().exists();
        assert!(predicate.has_field_provenance());
        match predicate.into_inner() {
            BasicPredicate::Field(fp) => {
                assert_eq!(fp.field_name(), "payload");
                assert_eq!(fp.op(), sassi::LookupOp::Json);
            }
            other => panic!("expected Field predicate, got {other:?}"),
        }
    }

    /// `.jsahibon.path("a.b").value::<i64>.gte(4)` builds a typed
    /// scalar comparison.
    #[test]
    fn path_value_gte_builds_scalar_compare() {
        let predicate = payload_field()
            .jsahibon()
            .path("engine.cylinders")
            .value::<i64>()
            .gte(4);
        assert!(predicate.has_field_provenance());
    }

    /// `.jsahibon.key("content-type").eq(...)` accepts non-identifier
    /// keys via the `key(...)` route.
    #[test]
    fn key_accepts_arbitrary_string() {
        let predicate = payload_field()
            .jsahibon()
            .key("content-type")
            .value::<String>()
            .eq("application/json".to_string());
        assert!(predicate.has_field_provenance());
    }

    /// `.jsahibon.path_segments([...]).exists` accepts arbitrary
    /// literal segments — non-identifier strings, digits, dots-in-keys.
    #[test]
    fn path_segments_accepts_arbitrary_segments() {
        let predicate = payload_field()
            .jsahibon()
            .path_segments(["a.b", "0", "cafe"])
            .exists();
        assert!(predicate.has_field_provenance());
    }

    /// `Option<MirJzSON>` builders carry the same provenance and surface.
    #[test]
    fn option_field_builds_predicates() {
        let predicate = maybe_field().jsahibon().missing();
        assert!(predicate.has_field_provenance());
    }

    /// `u64::MAX` operand survives the full builder pipeline through
    /// Sassi's `JScalarValue::U64` carrier.
    #[test]
    fn u64_max_operand_round_trips() {
        let predicate = payload_field()
            .jsahibon()
            .path("count")
            .value::<u64>()
            .eq(u64::MAX);
        assert!(predicate.has_field_provenance());
    }

    // ── SQL emission parity tests ─────────────────────────────────────────
    // The unit tests below construct a `PortablePredicate<Fake>` through
    // the Djogi builder, lower it to a `BasicPredicate<Fake>`, and emit
    // SQL via `query::portable::emit_basic_predicate`. They pin the
    // spec's SQL shape — two-valued boolean guards, bound parameters,
    // safe numeric preflight — without a live database.

    use crate::pg::accumulator::SqlAccumulator;
    use crate::query::SqlEmitContext;
    use crate::query::portable::{JsonTrust, emit_basic_predicate};

    fn emit_predicate(predicate: PortablePredicate<Fake>) -> String {
        let mut acc = SqlAccumulator::new("");
        let bp = predicate.into_inner();
        // The predicate came from a `PortablePredicate<Fake>` built via
        // the MirJzSON trusted-provenance builder, so pass
        // `JsonTrust::Trusted`. The new untrusted-rejection coverage
        // lives in `query::portable`'s test module.
        emit_basic_predicate::<Fake>(&mut acc, &bp, SqlEmitContext::root(), JsonTrust::Trusted)
            .expect("portable JSON predicate must emit SQL");
        let (sql, _binds) = acc.into_parts();
        sql
    }

    /// `exists` at root emits `(column #> $path_text_array) IS NOT NULL`
    /// per the spec's "Predicate mapping requirements" table.
    #[test]
    fn sql_exists_at_root_emits_path_extraction_is_not_null() {
        let predicate = payload_field().jsahibon().exists();
        let sql = emit_predicate(predicate);
        assert_eq!(sql, "(payload #> $1) IS NOT NULL");
    }

    /// `missing` at root emits the dual.
    #[test]
    fn sql_missing_at_root_emits_path_extraction_is_null() {
        let predicate = payload_field().jsahibon().missing();
        let sql = emit_predicate(predicate);
        assert_eq!(sql, "(payload #> $1) IS NULL");
    }

    /// `is_json_null` emits `COALESCE(j = 'null'::jsonb, FALSE)`
    /// note the `COALESCE` so missing/NULL projects to `FALSE` (not
    /// `NULL`), and the `'null'::jsonb` so JSON `null` is matched
    /// distinctly from SQL `NULL`.
    #[test]
    fn sql_is_json_null_uses_jsonb_null_literal_with_coalesce() {
        let predicate = payload_field().jsahibon().is_json_null();
        let sql = emit_predicate(predicate);
        assert_eq!(sql, "COALESCE((payload #> $1) = 'null'::jsonb, FALSE)");
    }

    /// `has_key("content-type")` guards `jsonb_typeof = 'object'` and
    /// binds the key as a parameter — never interpolates.
    #[test]
    fn sql_has_key_guards_object_type_and_binds_key() {
        let predicate = payload_field().jsahibon().has_key("content-type");
        let sql = emit_predicate(predicate);
        assert_eq!(
            sql,
            "COALESCE(jsonb_typeof((payload #> $1)) = 'object' AND (payload #> $2) ? $3, FALSE)"
        );
    }

    /// `has_any_key([...])` binds the keys as a `text[]` parameter and
    /// uses Postgres's `?|` operator with the object-type guard.
    #[test]
    fn sql_has_any_key_emits_key_array_bind() {
        let predicate = payload_field().jsahibon().has_any_key(["a", "b"]);
        let sql = emit_predicate(predicate);
        assert_eq!(
            sql,
            "COALESCE(jsonb_typeof((payload #> $1)) = 'object' AND (payload #> $2) ?| $3, FALSE)"
        );
    }

    /// Numeric scalar comparison emits the safe `CASE WHEN jsonb_typeof
    /// = 'number' THEN (j #>> '{}')::numeric op $operand ELSE FALSE END`
    /// shape. The cast happens only inside the type-guarded branch so
    /// non-numbers never trigger a cast error.
    #[test]
    fn sql_numeric_gte_uses_safe_case_with_numeric_cast() {
        let predicate = payload_field()
            .jsahibon()
            .path("count")
            .value::<i64>()
            .gte(4);
        let sql = emit_predicate(predicate);
        assert_eq!(
            sql,
            "CASE WHEN jsonb_typeof((payload #> $1)) = 'number' THEN ((payload #> $2) #>> '{}'::text[])::numeric >= $3 ELSE FALSE END"
        );
    }

    /// Array length comparison guards `jsonb_typeof = 'array'` and only
    /// calls `jsonb_array_length` inside that branch, so non-arrays
    /// return `FALSE` without erroring on the length call.
    #[test]
    fn sql_array_len_eq_guards_array_type_before_length_call() {
        let predicate = payload_field().jsahibon().array_len_eq(3);
        let sql = emit_predicate(predicate);
        assert_eq!(
            sql,
            "CASE WHEN jsonb_typeof((payload #> $1)) = 'array' THEN jsonb_array_length((payload #> $2)) = $3 ELSE FALSE END"
        );
    }

    /// `JsonEq` uses `COALESCE(j = $jsonb, FALSE)`. The `COALESCE` is
    /// mandatory — without it, SQL `NULL` on a missing path would
    /// propagate to NULL through the `=` operator (Postgres NULL
    /// equality returns NULL, not FALSE).
    #[test]
    fn sql_json_eq_wraps_in_coalesce() {
        let predicate = payload_field().jsahibon().eq_json(sassi::JSahibON::I64(42));
        let sql = emit_predicate(predicate);
        assert_eq!(sql, "COALESCE((payload #> $1) = $2, FALSE)");
    }

    /// `array_contains` builds a single-element JSON array and uses
    /// Postgres's `@>` operator with the array-type guard.
    #[test]
    fn sql_array_contains_uses_single_element_array_with_at_at_arrow() {
        let predicate = payload_field()
            .jsahibon()
            .array_contains(sassi::JSahibON::I64(7));
        let sql = emit_predicate(predicate);
        assert_eq!(
            sql,
            "COALESCE(jsonb_typeof((payload #> $1)) = 'array' AND (payload #> $2) @> $3, FALSE)"
        );
    }

    /// `Option<MirJzSON>` predicates emit the same SQL shape as
    /// required-field predicates — `missing` at root tests the
    /// optional field for NULL.
    #[test]
    fn sql_option_missing_emits_is_null() {
        let predicate = maybe_field().jsahibon().missing();
        let sql = emit_predicate(predicate);
        assert_eq!(sql, "(maybe #> $1) IS NULL");
    }

    /// Bind count parity — `exists` has one bind (the path array),
    /// `eq_json` has two (path + JSON value), `has_key` has three
    /// (path-guard + path-target + key). The accumulator's bind count
    /// is the canonical signal for parameter alignment.
    #[test]
    fn sql_bind_count_matches_spec() {
        fn count_binds(predicate: PortablePredicate<Fake>) -> usize {
            let mut acc = SqlAccumulator::new("");
            let bp = predicate.into_inner();
            // Same trust rationale as `emit_predicate` above — the
            // predicate originated at the MirJzSON trusted boundary.
            emit_basic_predicate::<Fake>(&mut acc, &bp, SqlEmitContext::root(), JsonTrust::Trusted)
                .unwrap();
            let (_sql, binds) = acc.into_parts();
            binds.len()
        }
        assert_eq!(count_binds(payload_field().jsahibon().exists()), 1);
        assert_eq!(
            count_binds(payload_field().jsahibon().eq_json(sassi::JSahibON::I64(42))),
            2
        );
        assert_eq!(count_binds(payload_field().jsahibon().has_key("x")), 3);
        // `u64::MAX` binds through Decimal — confirm the bind path
        // does not panic. The CASE expression has 3 binds: two `path`
        // binds + the Decimal operand.
        assert_eq!(
            count_binds(
                payload_field()
                    .jsahibon()
                    .path("c")
                    .value::<u64>()
                    .eq(u64::MAX),
            ),
            3
        );
    }

    /// `is_not_json_null` emits `COALESCE((j) <> 'null'::jsonb, FALSE)`
    /// the dual of `is_json_null` per the spec. Missing path / SQL
    /// NULL on the `<>` arm coalesces to `FALSE`, so the predicate stays
    /// two-valued under composition.
    #[test]
    fn sql_is_not_json_null_uses_coalesce_with_jsonb_null() {
        let predicate = payload_field().jsahibon().is_not_json_null();
        let sql = emit_predicate(predicate);
        assert_eq!(sql, "COALESCE((payload #> $1) <> 'null'::jsonb, FALSE)");
    }

    /// `between(low, high)` emits the safe `CASE` shape with
    /// `numeric BETWEEN $low AND $high` — the spec's required guarded
    /// numeric preflight. Non-numeric / missing path returns `FALSE`
    /// without erroring on the cast.
    #[test]
    fn sql_between_uses_safe_case_with_numeric_cast() {
        let predicate = payload_field()
            .jsahibon()
            .path("count")
            .value::<i64>()
            .between(1, 10);
        let sql = emit_predicate(predicate);
        // Note the THEN arm carries an extra opening `(` (matched by the
        // closing `)` before ` ELSE`) — the wrapper protects the
        // `BETWEEN` precedence against the outer `CASE WHEN ... THEN`.
        assert_eq!(
            sql,
            "CASE WHEN jsonb_typeof((payload #> $1)) = 'number' THEN \
             (((payload #> $2) #>> '{}'::text[])::numeric BETWEEN $3 AND $4) \
             ELSE FALSE END"
        );
    }

    /// `not_in([])` on a numeric path emits `TRUE` *inside* the
    /// `jsonb_typeof = 'number'` guard, but `FALSE` outside it — so a
    /// numeric-not-in-empty on a non-numeric / missing path is
    /// `FALSE`, not `TRUE`. This matches Sassi's "kind-guard first,
    /// empty-list identity second" rule and is the spec's required
    /// empty-set truth-table parity.
    #[test]
    fn sql_not_in_empty_returns_false_outside_kind_guard() {
        let predicate = payload_field()
            .jsahibon()
            .path("count")
            .value::<i64>()
            .not_in(Vec::<i64>::new());
        let sql = emit_predicate(predicate);
        assert_eq!(
            sql,
            "CASE WHEN jsonb_typeof((payload #> $1)) = 'number' THEN TRUE ELSE FALSE END"
        );
    }

    /// `in_([])` on a numeric path emits `FALSE` both inside and outside
    /// the kind guard — the only predicate that's `FALSE` everywhere.
    #[test]
    fn sql_in_empty_returns_false_everywhere() {
        let predicate = payload_field()
            .jsahibon()
            .path("count")
            .value::<i64>()
            .in_(Vec::<i64>::new());
        let sql = emit_predicate(predicate);
        assert_eq!(
            sql,
            "CASE WHEN jsonb_typeof((payload #> $1)) = 'number' THEN FALSE ELSE FALSE END"
        );
    }

    /// `has_all_keys([...])` guards `jsonb_typeof = 'object'` and uses
    /// Postgres's `?&` operator with the key array bound as `text[]`.
    /// Mirror of `has_any_key` but with the `?&` (all) variant.
    #[test]
    fn sql_has_all_keys_emits_object_guard_and_key_array() {
        let predicate = payload_field().jsahibon().has_all_keys(["a", "b"]);
        let sql = emit_predicate(predicate);
        assert_eq!(
            sql,
            "COALESCE(jsonb_typeof((payload #> $1)) = 'object' AND (payload #> $2) ?& $3, FALSE)"
        );
    }

    /// `is_type(Object)` short-circuits to a `COALESCE(jsonb_typeof =
    /// 'object', FALSE)` shape — distinct from `has_key` because no
    /// key predicate is composed on top. Mirrored by `is_bool`,
    /// `is_number`, etc. through the same `Type` body.
    #[test]
    fn sql_is_object_emits_jsonb_typeof_check() {
        let predicate = payload_field().jsahibon().is_object();
        let sql = emit_predicate(predicate);
        assert_eq!(
            sql,
            "COALESCE(jsonb_typeof((payload #> $1)) = 'object', FALSE)"
        );
    }
}
