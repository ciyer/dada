use dada_ir_ast::diagnostic::Errors;
use dada_util::boxed_async_fn;

use crate::{
    check::{
        chains::Lien,
        combinator::{exists, require, require_both},
        env::Env,
        places::PlaceTy,
        predicates::{
            Predicate,
            var_infer::{require_infer_is, require_var_is},
        },
        report::{Because, OrElse},
    },
    ir::{
        classes::SymAggregateStyle,
        types::{SymGenericTerm, SymPerm, SymPermKind, SymPlace, SymTy, SymTyKind, SymTyName},
    },
};

use super::is_provably_move::{place_is_provably_move, term_is_provably_move};

pub(crate) async fn require_term_is_move<'db>(
    env: &Env<'db>,
    term: SymGenericTerm<'db>,
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    match term {
        SymGenericTerm::Type(sym_ty) => require_ty_is_move(env, sym_ty, or_else).await,
        SymGenericTerm::Perm(sym_perm) => require_perm_is_move(env, sym_perm, or_else).await,
        SymGenericTerm::Place(place) => panic!("unexpected place term: {place:?}"),
        SymGenericTerm::Error(reported) => Err(reported),
    }
}

/// Requires that the given chain is `move`.
pub(crate) async fn require_chain_is_move<'db>(
    env: &Env<'db>,
    chain: &[Lien<'db>],
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    let db = env.db();
    let perm = Lien::chain_to_perm(db, chain);
    require_perm_is_move(env, perm, or_else).await
}

/// Requires that `(lhs rhs)` is `move`.
/// This requires both `lhs` and `rhs` to be `move` independently.
async fn require_application_is_move<'db>(
    env: &Env<'db>,
    lhs: SymGenericTerm<'db>,
    rhs: SymGenericTerm<'db>,
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    // Simultaneously test for whether LHS/RHS is `predicate`.
    // If either is, we are done.
    // If either is *not*, the other must be.
    require_both(
        require_term_is_move(env, lhs, or_else),
        require_term_is_move(env, rhs, or_else),
    )
    .await
}

#[boxed_async_fn]
async fn require_ty_is_move<'db>(
    env: &Env<'db>,
    term: SymTy<'db>,
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    let db = env.db();
    match *term.kind(db) {
        // Error cases first
        SymTyKind::Error(reported) => Err(reported),

        // Apply
        SymTyKind::Perm(sym_perm, sym_ty) => {
            require_application_is_move(env, sym_perm.into(), sym_ty.into(), or_else).await
        }

        // Never
        SymTyKind::Never => Ok(()),

        // Variable and inference
        SymTyKind::Infer(infer) => require_infer_is(env, infer, Predicate::Move, or_else),
        SymTyKind::Var(var) => require_var_is(env, var, Predicate::Move, or_else),

        // Named types
        SymTyKind::Named(sym_ty_name, ref generics) => match sym_ty_name {
            SymTyName::Primitive(prim) => Err(or_else.report(env, Because::PrimitiveIsCopy(prim))),

            SymTyName::Aggregate(sym_aggregate) => match sym_aggregate.style(db) {
                SymAggregateStyle::Class => Ok(()),
                SymAggregateStyle::Struct => {
                    require(
                        exists(generics, async |&generic| {
                            term_is_provably_move(env, generic).await
                        }),
                        || or_else.report(env, Because::JustSo),
                    )
                    .await
                }
            },

            SymTyName::Future => Ok(()),

            SymTyName::Tuple { arity } => {
                assert_eq!(arity, generics.len());
                require(
                    exists(generics, async |&generic| {
                        term_is_provably_move(env, generic).await
                    }),
                    || or_else.report(env, Because::JustSo),
                )
                .await
            }
        },
    }
}

#[boxed_async_fn]
async fn require_perm_is_move<'db>(
    env: &Env<'db>,
    perm: SymPerm<'db>,
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    let db = env.db();
    match *perm.kind(db) {
        SymPermKind::Error(reported) => Err(reported),

        SymPermKind::My => Ok(()),

        SymPermKind::Our => Err(or_else.report(env, Because::JustSo)),

        SymPermKind::Shared(_) => Err(or_else.report(env, Because::JustSo)),

        SymPermKind::Leased(ref places) => {
            // If there is at least one place `p` that is move, this will result in a `leased[p]` chain.
            require(
                exists(places, async |&place| {
                    place_is_provably_move(env, place).await
                }),
                || or_else.report(env, Because::LeasedFromCopyIsCopy(places.to_vec())),
            )
            .await
        }

        // Apply
        SymPermKind::Apply(lhs, rhs) => {
            require_application_is_move(env, lhs.into(), rhs.into(), or_else).await
        }

        // Variable and inference
        SymPermKind::Var(var) => require_var_is(env, var, Predicate::Move, or_else),
        SymPermKind::Infer(infer) => require_infer_is(env, infer, Predicate::Move, or_else),
    }
}

pub(super) async fn require_place_is_move<'db>(
    env: &Env<'db>,
    place: SymPlace<'db>,
    or_else: &dyn OrElse<'db>,
) -> Errors<()> {
    let ty = place.place_ty(env).await;
    require_ty_is_move(env, ty, or_else).await
}
