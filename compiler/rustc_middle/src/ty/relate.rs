//! Generalized type relating mechanism.
//!
//! A type relation `R` relates a pair of values `(A, B)`. `A and B` are usually
//! types or regions but can be other things. Examples of type relations are
//! subtyping, type equality, etc.

use crate::ty::error::{ExpectedFound, TypeError};
use crate::ty::{
    self, ExistentialPredicate, ExistentialPredicateStableCmpExt as _, GenericArg, GenericArgKind,
    GenericArgsRef, ImplSubject, Term, TermKind, Ty, TyCtxt, TypeFoldable,
};
use rustc_hir as hir;
use rustc_hir::def_id::DefId;
use rustc_macros::TypeVisitable;
use rustc_target::spec::abi;
use std::iter;
use tracing::{debug, instrument};

use super::Pattern;

pub type RelateResult<'tcx, T> = Result<T, TypeError<'tcx>>;

pub trait TypeRelation<'tcx>: Sized {
    fn tcx(&self) -> TyCtxt<'tcx>;

    /// Returns a static string we can use for printouts.
    fn tag(&self) -> &'static str;

    /// Generic relation routine suitable for most anything.
    fn relate<T: Relate<'tcx>>(&mut self, a: T, b: T) -> RelateResult<'tcx, T> {
        Relate::relate(self, a, b)
    }

    /// Relate the two args for the given item. The default
    /// is to look up the variance for the item and proceed
    /// accordingly.
    fn relate_item_args(
        &mut self,
        item_def_id: DefId,
        a_arg: GenericArgsRef<'tcx>,
        b_arg: GenericArgsRef<'tcx>,
    ) -> RelateResult<'tcx, GenericArgsRef<'tcx>> {
        debug!(
            "relate_item_args(item_def_id={:?}, a_arg={:?}, b_arg={:?})",
            item_def_id, a_arg, b_arg
        );

        let tcx = self.tcx();
        let opt_variances = tcx.variances_of(item_def_id);
        relate_args_with_variances(self, item_def_id, opt_variances, a_arg, b_arg, true)
    }

    /// Switch variance for the purpose of relating `a` and `b`.
    fn relate_with_variance<T: Relate<'tcx>>(
        &mut self,
        variance: ty::Variance,
        info: ty::VarianceDiagInfo<'tcx>,
        a: T,
        b: T,
    ) -> RelateResult<'tcx, T>;

    // Overridable relations. You shouldn't typically call these
    // directly, instead call `relate()`, which in turn calls
    // these. This is both more uniform but also allows us to add
    // additional hooks for other types in the future if needed
    // without making older code, which called `relate`, obsolete.

    fn tys(&mut self, a: Ty<'tcx>, b: Ty<'tcx>) -> RelateResult<'tcx, Ty<'tcx>>;

    fn regions(
        &mut self,
        a: ty::Region<'tcx>,
        b: ty::Region<'tcx>,
    ) -> RelateResult<'tcx, ty::Region<'tcx>>;

    fn consts(
        &mut self,
        a: ty::Const<'tcx>,
        b: ty::Const<'tcx>,
    ) -> RelateResult<'tcx, ty::Const<'tcx>>;

    fn binders<T>(
        &mut self,
        a: ty::Binder<'tcx, T>,
        b: ty::Binder<'tcx, T>,
    ) -> RelateResult<'tcx, ty::Binder<'tcx, T>>
    where
        T: Relate<'tcx>;
}

pub trait Relate<'tcx>: TypeFoldable<TyCtxt<'tcx>> + PartialEq + Copy {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: Self,
        b: Self,
    ) -> RelateResult<'tcx, Self>;
}

///////////////////////////////////////////////////////////////////////////
// Relate impls

#[inline]
pub fn relate_args_invariantly<'tcx, R: TypeRelation<'tcx>>(
    relation: &mut R,
    a_arg: GenericArgsRef<'tcx>,
    b_arg: GenericArgsRef<'tcx>,
) -> RelateResult<'tcx, GenericArgsRef<'tcx>> {
    relation.tcx().mk_args_from_iter(iter::zip(a_arg, b_arg).map(|(a, b)| {
        relation.relate_with_variance(ty::Invariant, ty::VarianceDiagInfo::default(), a, b)
    }))
}

pub fn relate_args_with_variances<'tcx, R: TypeRelation<'tcx>>(
    relation: &mut R,
    ty_def_id: DefId,
    variances: &[ty::Variance],
    a_arg: GenericArgsRef<'tcx>,
    b_arg: GenericArgsRef<'tcx>,
    fetch_ty_for_diag: bool,
) -> RelateResult<'tcx, GenericArgsRef<'tcx>> {
    let tcx = relation.tcx();

    let mut cached_ty = None;
    let params = iter::zip(a_arg, b_arg).enumerate().map(|(i, (a, b))| {
        let variance = variances[i];
        let variance_info = if variance == ty::Invariant && fetch_ty_for_diag {
            let ty =
                *cached_ty.get_or_insert_with(|| tcx.type_of(ty_def_id).instantiate(tcx, a_arg));
            ty::VarianceDiagInfo::Invariant { ty, param_index: i.try_into().unwrap() }
        } else {
            ty::VarianceDiagInfo::default()
        };
        relation.relate_with_variance(variance, variance_info, a, b)
    });

    tcx.mk_args_from_iter(params)
}

impl<'tcx> Relate<'tcx> for ty::FnSig<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ty::FnSig<'tcx>,
        b: ty::FnSig<'tcx>,
    ) -> RelateResult<'tcx, ty::FnSig<'tcx>> {
        let tcx = relation.tcx();

        if a.c_variadic != b.c_variadic {
            return Err(TypeError::VariadicMismatch(expected_found(a.c_variadic, b.c_variadic)));
        }
        let safety = relation.relate(a.safety, b.safety)?;
        let abi = relation.relate(a.abi, b.abi)?;

        if a.inputs().len() != b.inputs().len() {
            return Err(TypeError::ArgCount);
        }

        let inputs_and_output = iter::zip(a.inputs(), b.inputs())
            .map(|(&a, &b)| ((a, b), false))
            .chain(iter::once(((a.output(), b.output()), true)))
            .map(|((a, b), is_output)| {
                if is_output {
                    relation.relate(a, b)
                } else {
                    relation.relate_with_variance(
                        ty::Contravariant,
                        ty::VarianceDiagInfo::default(),
                        a,
                        b,
                    )
                }
            })
            .enumerate()
            .map(|(i, r)| match r {
                Err(TypeError::Sorts(exp_found) | TypeError::ArgumentSorts(exp_found, _)) => {
                    Err(TypeError::ArgumentSorts(exp_found, i))
                }
                Err(TypeError::Mutability | TypeError::ArgumentMutability(_)) => {
                    Err(TypeError::ArgumentMutability(i))
                }
                r => r,
            });
        Ok(ty::FnSig {
            inputs_and_output: tcx.mk_type_list_from_iter(inputs_and_output)?,
            c_variadic: a.c_variadic,
            safety,
            abi,
        })
    }
}

impl<'tcx> Relate<'tcx> for ty::BoundConstness {
    fn relate<R: TypeRelation<'tcx>>(
        _relation: &mut R,
        a: ty::BoundConstness,
        b: ty::BoundConstness,
    ) -> RelateResult<'tcx, ty::BoundConstness> {
        if a != b { Err(TypeError::ConstnessMismatch(expected_found(a, b))) } else { Ok(a) }
    }
}

impl<'tcx> Relate<'tcx> for hir::Safety {
    fn relate<R: TypeRelation<'tcx>>(
        _relation: &mut R,
        a: hir::Safety,
        b: hir::Safety,
    ) -> RelateResult<'tcx, hir::Safety> {
        if a != b { Err(TypeError::SafetyMismatch(expected_found(a, b))) } else { Ok(a) }
    }
}

impl<'tcx> Relate<'tcx> for abi::Abi {
    fn relate<R: TypeRelation<'tcx>>(
        _relation: &mut R,
        a: abi::Abi,
        b: abi::Abi,
    ) -> RelateResult<'tcx, abi::Abi> {
        if a == b { Ok(a) } else { Err(TypeError::AbiMismatch(expected_found(a, b))) }
    }
}

impl<'tcx> Relate<'tcx> for ty::AliasTy<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ty::AliasTy<'tcx>,
        b: ty::AliasTy<'tcx>,
    ) -> RelateResult<'tcx, ty::AliasTy<'tcx>> {
        if a.def_id != b.def_id {
            Err(TypeError::ProjectionMismatched(expected_found(a.def_id, b.def_id)))
        } else {
            let args = match a.kind(relation.tcx()) {
                ty::Opaque => relate_args_with_variances(
                    relation,
                    a.def_id,
                    relation.tcx().variances_of(a.def_id),
                    a.args,
                    b.args,
                    false, // do not fetch `type_of(a_def_id)`, as it will cause a cycle
                )?,
                ty::Projection | ty::Weak | ty::Inherent => {
                    relate_args_invariantly(relation, a.args, b.args)?
                }
            };
            Ok(ty::AliasTy::new(relation.tcx(), a.def_id, args))
        }
    }
}

impl<'tcx> Relate<'tcx> for ty::AliasTerm<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ty::AliasTerm<'tcx>,
        b: ty::AliasTerm<'tcx>,
    ) -> RelateResult<'tcx, ty::AliasTerm<'tcx>> {
        if a.def_id != b.def_id {
            Err(TypeError::ProjectionMismatched(expected_found(a.def_id, b.def_id)))
        } else {
            let args = match a.kind(relation.tcx()) {
                ty::AliasTermKind::OpaqueTy => relate_args_with_variances(
                    relation,
                    a.def_id,
                    relation.tcx().variances_of(a.def_id),
                    a.args,
                    b.args,
                    false, // do not fetch `type_of(a_def_id)`, as it will cause a cycle
                )?,
                ty::AliasTermKind::ProjectionTy
                | ty::AliasTermKind::WeakTy
                | ty::AliasTermKind::InherentTy
                | ty::AliasTermKind::UnevaluatedConst
                | ty::AliasTermKind::ProjectionConst => {
                    relate_args_invariantly(relation, a.args, b.args)?
                }
            };
            Ok(ty::AliasTerm::new(relation.tcx(), a.def_id, args))
        }
    }
}

impl<'tcx> Relate<'tcx> for ty::ExistentialProjection<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ty::ExistentialProjection<'tcx>,
        b: ty::ExistentialProjection<'tcx>,
    ) -> RelateResult<'tcx, ty::ExistentialProjection<'tcx>> {
        if a.def_id != b.def_id {
            Err(TypeError::ProjectionMismatched(expected_found(a.def_id, b.def_id)))
        } else {
            let term = relation.relate_with_variance(
                ty::Invariant,
                ty::VarianceDiagInfo::default(),
                a.term,
                b.term,
            )?;
            let args = relation.relate_with_variance(
                ty::Invariant,
                ty::VarianceDiagInfo::default(),
                a.args,
                b.args,
            )?;
            Ok(ty::ExistentialProjection { def_id: a.def_id, args, term })
        }
    }
}

impl<'tcx> Relate<'tcx> for ty::TraitRef<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ty::TraitRef<'tcx>,
        b: ty::TraitRef<'tcx>,
    ) -> RelateResult<'tcx, ty::TraitRef<'tcx>> {
        // Different traits cannot be related.
        if a.def_id != b.def_id {
            Err(TypeError::Traits(expected_found(a.def_id, b.def_id)))
        } else {
            let args = relate_args_invariantly(relation, a.args, b.args)?;
            Ok(ty::TraitRef::new(relation.tcx(), a.def_id, args))
        }
    }
}

impl<'tcx> Relate<'tcx> for ty::ExistentialTraitRef<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ty::ExistentialTraitRef<'tcx>,
        b: ty::ExistentialTraitRef<'tcx>,
    ) -> RelateResult<'tcx, ty::ExistentialTraitRef<'tcx>> {
        // Different traits cannot be related.
        if a.def_id != b.def_id {
            Err(TypeError::Traits(expected_found(a.def_id, b.def_id)))
        } else {
            let args = relate_args_invariantly(relation, a.args, b.args)?;
            Ok(ty::ExistentialTraitRef { def_id: a.def_id, args })
        }
    }
}

#[derive(PartialEq, Copy, Debug, Clone, TypeFoldable, TypeVisitable)]
struct CoroutineWitness<'tcx>(&'tcx ty::List<Ty<'tcx>>);

impl<'tcx> Relate<'tcx> for CoroutineWitness<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: CoroutineWitness<'tcx>,
        b: CoroutineWitness<'tcx>,
    ) -> RelateResult<'tcx, CoroutineWitness<'tcx>> {
        assert_eq!(a.0.len(), b.0.len());
        let tcx = relation.tcx();
        let types =
            tcx.mk_type_list_from_iter(iter::zip(a.0, b.0).map(|(a, b)| relation.relate(a, b)))?;
        Ok(CoroutineWitness(types))
    }
}

impl<'tcx> Relate<'tcx> for ImplSubject<'tcx> {
    #[inline]
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ImplSubject<'tcx>,
        b: ImplSubject<'tcx>,
    ) -> RelateResult<'tcx, ImplSubject<'tcx>> {
        match (a, b) {
            (ImplSubject::Trait(trait_ref_a), ImplSubject::Trait(trait_ref_b)) => {
                let trait_ref = ty::TraitRef::relate(relation, trait_ref_a, trait_ref_b)?;
                Ok(ImplSubject::Trait(trait_ref))
            }
            (ImplSubject::Inherent(ty_a), ImplSubject::Inherent(ty_b)) => {
                let ty = Ty::relate(relation, ty_a, ty_b)?;
                Ok(ImplSubject::Inherent(ty))
            }
            (ImplSubject::Trait(_), ImplSubject::Inherent(_))
            | (ImplSubject::Inherent(_), ImplSubject::Trait(_)) => {
                bug!("can not relate TraitRef and Ty");
            }
        }
    }
}

impl<'tcx> Relate<'tcx> for Ty<'tcx> {
    #[inline]
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: Ty<'tcx>,
        b: Ty<'tcx>,
    ) -> RelateResult<'tcx, Ty<'tcx>> {
        relation.tys(a, b)
    }
}

impl<'tcx> Relate<'tcx> for Pattern<'tcx> {
    #[inline]
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: Self,
        b: Self,
    ) -> RelateResult<'tcx, Self> {
        match (&*a, &*b) {
            (
                &ty::PatternKind::Range { start: start_a, end: end_a, include_end: inc_a },
                &ty::PatternKind::Range { start: start_b, end: end_b, include_end: inc_b },
            ) => {
                // FIXME(pattern_types): make equal patterns equal (`0..=` is the same as `..=`).
                let mut relate_opt_const = |a, b| match (a, b) {
                    (None, None) => Ok(None),
                    (Some(a), Some(b)) => relation.relate(a, b).map(Some),
                    // FIXME(pattern_types): report a better error
                    _ => Err(TypeError::Mismatch),
                };
                let start = relate_opt_const(start_a, start_b)?;
                let end = relate_opt_const(end_a, end_b)?;
                if inc_a != inc_b {
                    todo!()
                }
                Ok(relation.tcx().mk_pat(ty::PatternKind::Range { start, end, include_end: inc_a }))
            }
        }
    }
}

/// Relates `a` and `b` structurally, calling the relation for all nested values.
/// Any semantic equality, e.g. of projections, and inference variables have to be
/// handled by the caller.
#[instrument(level = "trace", skip(relation), ret)]
pub fn structurally_relate_tys<'tcx, R: TypeRelation<'tcx>>(
    relation: &mut R,
    a: Ty<'tcx>,
    b: Ty<'tcx>,
) -> RelateResult<'tcx, Ty<'tcx>> {
    let tcx = relation.tcx();
    match (a.kind(), b.kind()) {
        (&ty::Infer(_), _) | (_, &ty::Infer(_)) => {
            // The caller should handle these cases!
            bug!("var types encountered in structurally_relate_tys")
        }

        (ty::Bound(..), _) | (_, ty::Bound(..)) => {
            bug!("bound types encountered in structurally_relate_tys")
        }

        (&ty::Error(guar), _) | (_, &ty::Error(guar)) => Ok(Ty::new_error(tcx, guar)),

        (&ty::Never, _)
        | (&ty::Char, _)
        | (&ty::Bool, _)
        | (&ty::Int(_), _)
        | (&ty::Uint(_), _)
        | (&ty::Float(_), _)
        | (&ty::Str, _)
            if a == b =>
        {
            Ok(a)
        }

        (ty::Param(a_p), ty::Param(b_p)) if a_p.index == b_p.index => {
            debug_assert_eq!(a_p.name, b_p.name, "param types with same index differ in name");
            Ok(a)
        }

        (ty::Placeholder(p1), ty::Placeholder(p2)) if p1 == p2 => Ok(a),

        (&ty::Adt(a_def, a_args), &ty::Adt(b_def, b_args)) if a_def == b_def => {
            let args = relation.relate_item_args(a_def.did(), a_args, b_args)?;
            Ok(Ty::new_adt(tcx, a_def, args))
        }

        (&ty::Foreign(a_id), &ty::Foreign(b_id)) if a_id == b_id => Ok(Ty::new_foreign(tcx, a_id)),

        (&ty::Dynamic(a_obj, a_region, a_repr), &ty::Dynamic(b_obj, b_region, b_repr))
            if a_repr == b_repr =>
        {
            Ok(Ty::new_dynamic(
                tcx,
                relation.relate(a_obj, b_obj)?,
                relation.relate(a_region, b_region)?,
                a_repr,
            ))
        }

        (&ty::Coroutine(a_id, a_args), &ty::Coroutine(b_id, b_args)) if a_id == b_id => {
            // All Coroutine types with the same id represent
            // the (anonymous) type of the same coroutine expression. So
            // all of their regions should be equated.
            let args = relate_args_invariantly(relation, a_args, b_args)?;
            Ok(Ty::new_coroutine(tcx, a_id, args))
        }

        (&ty::CoroutineWitness(a_id, a_args), &ty::CoroutineWitness(b_id, b_args))
            if a_id == b_id =>
        {
            // All CoroutineWitness types with the same id represent
            // the (anonymous) type of the same coroutine expression. So
            // all of their regions should be equated.
            let args = relate_args_invariantly(relation, a_args, b_args)?;
            Ok(Ty::new_coroutine_witness(tcx, a_id, args))
        }

        (&ty::Closure(a_id, a_args), &ty::Closure(b_id, b_args)) if a_id == b_id => {
            // All Closure types with the same id represent
            // the (anonymous) type of the same closure expression. So
            // all of their regions should be equated.
            let args = relate_args_invariantly(relation, a_args, b_args)?;
            Ok(Ty::new_closure(tcx, a_id, args))
        }

        (&ty::CoroutineClosure(a_id, a_args), &ty::CoroutineClosure(b_id, b_args))
            if a_id == b_id =>
        {
            let args = relate_args_invariantly(relation, a_args, b_args)?;
            Ok(Ty::new_coroutine_closure(tcx, a_id, args))
        }

        (&ty::RawPtr(a_ty, a_mutbl), &ty::RawPtr(b_ty, b_mutbl)) => {
            if a_mutbl != b_mutbl {
                return Err(TypeError::Mutability);
            }

            let (variance, info) = match a_mutbl {
                hir::Mutability::Not => (ty::Covariant, ty::VarianceDiagInfo::None),
                hir::Mutability::Mut => {
                    (ty::Invariant, ty::VarianceDiagInfo::Invariant { ty: a, param_index: 0 })
                }
            };

            let ty = relation.relate_with_variance(variance, info, a_ty, b_ty)?;

            Ok(Ty::new_ptr(tcx, ty, a_mutbl))
        }

        (&ty::Ref(a_r, a_ty, a_mutbl), &ty::Ref(b_r, b_ty, b_mutbl)) => {
            if a_mutbl != b_mutbl {
                return Err(TypeError::Mutability);
            }

            let (variance, info) = match a_mutbl {
                hir::Mutability::Not => (ty::Covariant, ty::VarianceDiagInfo::None),
                hir::Mutability::Mut => {
                    (ty::Invariant, ty::VarianceDiagInfo::Invariant { ty: a, param_index: 0 })
                }
            };

            let r = relation.relate(a_r, b_r)?;
            let ty = relation.relate_with_variance(variance, info, a_ty, b_ty)?;

            Ok(Ty::new_ref(tcx, r, ty, a_mutbl))
        }

        (&ty::Array(a_t, sz_a), &ty::Array(b_t, sz_b)) => {
            let t = relation.relate(a_t, b_t)?;
            match relation.relate(sz_a, sz_b) {
                Ok(sz) => Ok(Ty::new_array_with_const_len(tcx, t, sz)),
                Err(err) => {
                    // Check whether the lengths are both concrete/known values,
                    // but are unequal, for better diagnostics.
                    let sz_a = sz_a.try_to_target_usize(tcx);
                    let sz_b = sz_b.try_to_target_usize(tcx);

                    match (sz_a, sz_b) {
                        (Some(sz_a_val), Some(sz_b_val)) if sz_a_val != sz_b_val => {
                            Err(TypeError::FixedArraySize(expected_found(sz_a_val, sz_b_val)))
                        }
                        _ => Err(err),
                    }
                }
            }
        }

        (&ty::Slice(a_t), &ty::Slice(b_t)) => {
            let t = relation.relate(a_t, b_t)?;
            Ok(Ty::new_slice(tcx, t))
        }

        (&ty::Tuple(as_), &ty::Tuple(bs)) => {
            if as_.len() == bs.len() {
                Ok(Ty::new_tup_from_iter(
                    tcx,
                    iter::zip(as_, bs).map(|(a, b)| relation.relate(a, b)),
                )?)
            } else if !(as_.is_empty() || bs.is_empty()) {
                Err(TypeError::TupleSize(expected_found(as_.len(), bs.len())))
            } else {
                Err(TypeError::Sorts(expected_found(a, b)))
            }
        }

        (&ty::FnDef(a_def_id, a_args), &ty::FnDef(b_def_id, b_args)) if a_def_id == b_def_id => {
            let args = relation.relate_item_args(a_def_id, a_args, b_args)?;
            Ok(Ty::new_fn_def(tcx, a_def_id, args))
        }

        (&ty::FnPtr(a_fty), &ty::FnPtr(b_fty)) => {
            let fty = relation.relate(a_fty, b_fty)?;
            Ok(Ty::new_fn_ptr(tcx, fty))
        }

        // Alias tend to mostly already be handled downstream due to normalization.
        (&ty::Alias(a_kind, a_data), &ty::Alias(b_kind, b_data)) => {
            let alias_ty = relation.relate(a_data, b_data)?;
            assert_eq!(a_kind, b_kind);
            Ok(Ty::new_alias(tcx, a_kind, alias_ty))
        }

        (&ty::Pat(a_ty, a_pat), &ty::Pat(b_ty, b_pat)) => {
            let ty = relation.relate(a_ty, b_ty)?;
            let pat = relation.relate(a_pat, b_pat)?;
            Ok(Ty::new_pat(tcx, ty, pat))
        }

        _ => Err(TypeError::Sorts(expected_found(a, b))),
    }
}

/// Relates `a` and `b` structurally, calling the relation for all nested values.
/// Any semantic equality, e.g. of unevaluated consts, and inference variables have
/// to be handled by the caller.
///
/// FIXME: This is not totally structual, which probably should be fixed.
/// See the HACKs below.
pub fn structurally_relate_consts<'tcx, R: TypeRelation<'tcx>>(
    relation: &mut R,
    mut a: ty::Const<'tcx>,
    mut b: ty::Const<'tcx>,
) -> RelateResult<'tcx, ty::Const<'tcx>> {
    debug!("{}.structurally_relate_consts(a = {:?}, b = {:?})", relation.tag(), a, b);
    let tcx = relation.tcx();

    if tcx.features().generic_const_exprs {
        a = tcx.expand_abstract_consts(a);
        b = tcx.expand_abstract_consts(b);
    }

    debug!("{}.structurally_relate_consts(normed_a = {:?}, normed_b = {:?})", relation.tag(), a, b);

    // Currently, the values that can be unified are primitive types,
    // and those that derive both `PartialEq` and `Eq`, corresponding
    // to structural-match types.
    let is_match = match (a.kind(), b.kind()) {
        (ty::ConstKind::Infer(_), _) | (_, ty::ConstKind::Infer(_)) => {
            // The caller should handle these cases!
            bug!("var types encountered in structurally_relate_consts: {:?} {:?}", a, b)
        }

        (ty::ConstKind::Error(_), _) => return Ok(a),
        (_, ty::ConstKind::Error(_)) => return Ok(b),

        (ty::ConstKind::Param(a_p), ty::ConstKind::Param(b_p)) if a_p.index == b_p.index => {
            debug_assert_eq!(a_p.name, b_p.name, "param types with same index differ in name");
            true
        }
        (ty::ConstKind::Placeholder(p1), ty::ConstKind::Placeholder(p2)) => p1 == p2,
        (ty::ConstKind::Value(_, a_val), ty::ConstKind::Value(_, b_val)) => a_val == b_val,

        // While this is slightly incorrect, it shouldn't matter for `min_const_generics`
        // and is the better alternative to waiting until `generic_const_exprs` can
        // be stabilized.
        (ty::ConstKind::Unevaluated(au), ty::ConstKind::Unevaluated(bu)) if au.def == bu.def => {
            if cfg!(debug_assertions) {
                let a_ty = tcx.type_of(au.def).instantiate(tcx, au.args);
                let b_ty = tcx.type_of(bu.def).instantiate(tcx, bu.args);
                assert_eq!(a_ty, b_ty);
            }

            let args = relation.relate_with_variance(
                ty::Variance::Invariant,
                ty::VarianceDiagInfo::default(),
                au.args,
                bu.args,
            )?;
            return Ok(ty::Const::new_unevaluated(tcx, ty::UnevaluatedConst { def: au.def, args }));
        }
        (ty::ConstKind::Expr(ae), ty::ConstKind::Expr(be)) => {
            match (ae.kind, be.kind) {
                (ty::ExprKind::Binop(a_binop), ty::ExprKind::Binop(b_binop))
                    if a_binop == b_binop => {}
                (ty::ExprKind::UnOp(a_unop), ty::ExprKind::UnOp(b_unop)) if a_unop == b_unop => {}
                (ty::ExprKind::FunctionCall, ty::ExprKind::FunctionCall) => {}
                (ty::ExprKind::Cast(a_kind), ty::ExprKind::Cast(b_kind)) if a_kind == b_kind => {}
                _ => return Err(TypeError::ConstMismatch(expected_found(a, b))),
            }

            let args = relation.relate(ae.args(), be.args())?;
            return Ok(ty::Const::new_expr(tcx, ty::Expr::new(ae.kind, args)));
        }
        _ => false,
    };
    if is_match { Ok(a) } else { Err(TypeError::ConstMismatch(expected_found(a, b))) }
}

impl<'tcx> Relate<'tcx> for &'tcx ty::List<ty::PolyExistentialPredicate<'tcx>> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: Self,
        b: Self,
    ) -> RelateResult<'tcx, Self> {
        let tcx = relation.tcx();

        // FIXME: this is wasteful, but want to do a perf run to see how slow it is.
        // We need to perform this deduplication as we sometimes generate duplicate projections
        // in `a`.
        let mut a_v: Vec<_> = a.into_iter().collect();
        let mut b_v: Vec<_> = b.into_iter().collect();
        // `skip_binder` here is okay because `stable_cmp` doesn't look at binders
        a_v.sort_by(|a, b| a.skip_binder().stable_cmp(tcx, &b.skip_binder()));
        a_v.dedup();
        b_v.sort_by(|a, b| a.skip_binder().stable_cmp(tcx, &b.skip_binder()));
        b_v.dedup();
        if a_v.len() != b_v.len() {
            return Err(TypeError::ExistentialMismatch(expected_found(a, b)));
        }

        let v = iter::zip(a_v, b_v).map(|(ep_a, ep_b)| {
            match (ep_a.skip_binder(), ep_b.skip_binder()) {
                (ExistentialPredicate::Trait(a), ExistentialPredicate::Trait(b)) => Ok(ep_a
                    .rebind(ExistentialPredicate::Trait(
                        relation.relate(ep_a.rebind(a), ep_b.rebind(b))?.skip_binder(),
                    ))),
                (ExistentialPredicate::Projection(a), ExistentialPredicate::Projection(b)) => {
                    Ok(ep_a.rebind(ExistentialPredicate::Projection(
                        relation.relate(ep_a.rebind(a), ep_b.rebind(b))?.skip_binder(),
                    )))
                }
                (ExistentialPredicate::AutoTrait(a), ExistentialPredicate::AutoTrait(b))
                    if a == b =>
                {
                    Ok(ep_a.rebind(ExistentialPredicate::AutoTrait(a)))
                }
                _ => Err(TypeError::ExistentialMismatch(expected_found(a, b))),
            }
        });
        tcx.mk_poly_existential_predicates_from_iter(v)
    }
}

impl<'tcx> Relate<'tcx> for GenericArgsRef<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: GenericArgsRef<'tcx>,
        b: GenericArgsRef<'tcx>,
    ) -> RelateResult<'tcx, GenericArgsRef<'tcx>> {
        relate_args_invariantly(relation, a, b)
    }
}

impl<'tcx> Relate<'tcx> for ty::Region<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ty::Region<'tcx>,
        b: ty::Region<'tcx>,
    ) -> RelateResult<'tcx, ty::Region<'tcx>> {
        relation.regions(a, b)
    }
}

impl<'tcx> Relate<'tcx> for ty::Const<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ty::Const<'tcx>,
        b: ty::Const<'tcx>,
    ) -> RelateResult<'tcx, ty::Const<'tcx>> {
        relation.consts(a, b)
    }
}

impl<'tcx, T: Relate<'tcx>> Relate<'tcx> for ty::Binder<'tcx, T> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ty::Binder<'tcx, T>,
        b: ty::Binder<'tcx, T>,
    ) -> RelateResult<'tcx, ty::Binder<'tcx, T>> {
        relation.binders(a, b)
    }
}

impl<'tcx> Relate<'tcx> for GenericArg<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: GenericArg<'tcx>,
        b: GenericArg<'tcx>,
    ) -> RelateResult<'tcx, GenericArg<'tcx>> {
        match (a.unpack(), b.unpack()) {
            (GenericArgKind::Lifetime(a_lt), GenericArgKind::Lifetime(b_lt)) => {
                Ok(relation.relate(a_lt, b_lt)?.into())
            }
            (GenericArgKind::Type(a_ty), GenericArgKind::Type(b_ty)) => {
                Ok(relation.relate(a_ty, b_ty)?.into())
            }
            (GenericArgKind::Const(a_ct), GenericArgKind::Const(b_ct)) => {
                Ok(relation.relate(a_ct, b_ct)?.into())
            }
            (GenericArgKind::Lifetime(unpacked), x) => {
                bug!("impossible case reached: can't relate: {:?} with {:?}", unpacked, x)
            }
            (GenericArgKind::Type(unpacked), x) => {
                bug!("impossible case reached: can't relate: {:?} with {:?}", unpacked, x)
            }
            (GenericArgKind::Const(unpacked), x) => {
                bug!("impossible case reached: can't relate: {:?} with {:?}", unpacked, x)
            }
        }
    }
}

impl<'tcx> Relate<'tcx> for ty::PredicatePolarity {
    fn relate<R: TypeRelation<'tcx>>(
        _relation: &mut R,
        a: ty::PredicatePolarity,
        b: ty::PredicatePolarity,
    ) -> RelateResult<'tcx, ty::PredicatePolarity> {
        if a != b { Err(TypeError::PolarityMismatch(expected_found(a, b))) } else { Ok(a) }
    }
}

impl<'tcx> Relate<'tcx> for ty::TraitPredicate<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: ty::TraitPredicate<'tcx>,
        b: ty::TraitPredicate<'tcx>,
    ) -> RelateResult<'tcx, ty::TraitPredicate<'tcx>> {
        Ok(ty::TraitPredicate {
            trait_ref: relation.relate(a.trait_ref, b.trait_ref)?,
            polarity: relation.relate(a.polarity, b.polarity)?,
        })
    }
}

impl<'tcx> Relate<'tcx> for Term<'tcx> {
    fn relate<R: TypeRelation<'tcx>>(
        relation: &mut R,
        a: Self,
        b: Self,
    ) -> RelateResult<'tcx, Self> {
        Ok(match (a.unpack(), b.unpack()) {
            (TermKind::Ty(a), TermKind::Ty(b)) => relation.relate(a, b)?.into(),
            (TermKind::Const(a), TermKind::Const(b)) => relation.relate(a, b)?.into(),
            _ => return Err(TypeError::Mismatch),
        })
    }
}

///////////////////////////////////////////////////////////////////////////
// Error handling

pub fn expected_found<T>(a: T, b: T) -> ExpectedFound<T> {
    ExpectedFound::new(true, a, b)
}
