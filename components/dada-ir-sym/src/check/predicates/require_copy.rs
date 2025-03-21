use dada_ir_ast::diagnostic::Errors;
use dada_util::boxed_async_fn;

use crate::{
    check::{
        env::Env,
        places::PlaceTy,
        predicates::{
            Predicate,
            var_infer::{require_infer_is, require_var_is},
        },
        red::Lien,
        report::{Because, OrElse},
    },
    ir::{
        classes::SymAggregateStyle,
        types::{SymGenericTerm, SymPerm, SymPermKind, SymPlace, SymTy, SymTyKind, SymTyName},
    },
};

use super::is_provably_copy::term_is_provably_copy;

pub(crate) async fn require_term_is_copy<'db>(
    env: &mut Env<'db>,
    term: SymGenericTerm<'db>,
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    match term {
        SymGenericTerm::Type(sym_ty) => require_ty_is_copy(env, sym_ty, or_else).await,
        SymGenericTerm::Perm(sym_perm) => require_perm_is_copy(env, sym_perm, or_else).await,
        SymGenericTerm::Place(place) => panic!("unexpected place term: {place:?}"),
        SymGenericTerm::Error(reported) => Err(reported),
    }
}

/// Requires that the given chain is `copy`.
pub(crate) async fn require_chain_is_copy<'db>(
    env: &mut Env<'db>,
    chain: &[Lien<'db>],
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    let db = env.db();
    let perm = Lien::chain_to_perm(db, chain);
    require_perm_is_copy(env, perm, or_else).await
}

/// Requires that `(lhs rhs)` satisfies the given predicate.
/// The semantics of `(lhs rhs)` is: `rhs` if `rhs is copy` or `lhs union rhs` otherwise.
async fn require_either_is_copy<'db>(
    env: &mut Env<'db>,
    lhs: SymGenericTerm<'db>,
    rhs: SymGenericTerm<'db>,
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    // Simultaneously test for whether LHS/RHS is `predicate`.
    // If either is, we are done.
    // If either is *not*, the other must be.
    env.require_both(
        async |env| {
            if !term_is_provably_copy(env, rhs).await? {
                require_term_is_copy(env, lhs, or_else).await?;
            }
            Ok(())
        },
        async |env| {
            if !term_is_provably_copy(env, lhs).await? {
                require_term_is_copy(env, rhs, or_else).await?;
            }
            Ok(())
        },
    )
    .await
}

#[boxed_async_fn]
async fn require_ty_is_copy<'db>(
    env: &mut Env<'db>,
    term: SymTy<'db>,
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    let db = env.db();
    match *term.kind(db) {
        // Error cases first
        SymTyKind::Error(reported) => Err(reported),

        // Apply
        SymTyKind::Perm(sym_perm, sym_ty) => {
            require_either_is_copy(env, sym_perm.into(), sym_ty.into(), or_else).await
        }

        // Never
        SymTyKind::Never => Err(or_else.report(env, Because::NeverIsNotCopy)),

        // Inference variables
        SymTyKind::Infer(infer) => require_infer_is(env, infer, Predicate::Copy, or_else),

        // Universal variables
        SymTyKind::Var(var) => require_var_is(env, var, Predicate::Copy, or_else),

        // Named types
        SymTyKind::Named(sym_ty_name, ref generics) => match sym_ty_name {
            SymTyName::Primitive(_) => Ok(()),

            SymTyName::Aggregate(sym_aggregate) => match sym_aggregate.style(db) {
                SymAggregateStyle::Class => {
                    Err(or_else.report(env, Because::ClassIsNotCopy(sym_ty_name)))
                }
                SymAggregateStyle::Struct => {
                    env.require_for_all(generics, async |env, &generic| {
                        require_term_is_copy(env, generic, or_else).await
                    })
                    .await
                }
            },

            SymTyName::Future => Err(or_else.report(env, Because::ClassIsNotCopy(sym_ty_name))),

            SymTyName::Tuple { arity } => {
                assert_eq!(arity, generics.len());
                env.require_for_all(generics, async |env, &generic| {
                    require_term_is_copy(env, generic, or_else).await
                })
                .await
            }
        },
    }
}

#[boxed_async_fn]
async fn require_perm_is_copy<'db>(
    env: &mut Env<'db>,
    perm: SymPerm<'db>,
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    let db = env.db();
    match *perm.kind(db) {
        SymPermKind::Error(reported) => Err(reported),

        SymPermKind::My => Err(or_else.report(env, Because::JustSo)),

        SymPermKind::Our => Ok(()),

        SymPermKind::Shared(_) => Ok(()),

        SymPermKind::Leased(ref places) => {
            // For a leased[p] to be copy, all the places in `p` must have copy permission.
            env.require_for_all(places, async |env, &place| {
                require_place_is_copy(env, place, or_else).await
            })
            .await
        }

        // Apply
        SymPermKind::Apply(lhs, rhs) => {
            require_either_is_copy(env, lhs.into(), rhs.into(), or_else).await
        }

        // Variable and inference
        SymPermKind::Var(var) => require_var_is(env, var, Predicate::Copy, or_else),
        SymPermKind::Infer(infer) => require_infer_is(env, infer, Predicate::Copy, or_else),
    }
}

pub(crate) async fn require_place_is_copy<'db>(
    env: &mut Env<'db>,
    place: SymPlace<'db>,
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    let ty = place.place_ty(env).await;
    require_ty_is_copy(env, ty, or_else).await
}
