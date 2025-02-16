use dada_ir_ast::diagnostic::Errors;
use dada_util::boxed_async_fn;

use crate::{
    check::{
        env::Env,
        places::PlaceTy,
        predicates::{
            Predicate,
            combinator::{either, for_all},
            var_infer::{test_infer_is, test_var_is},
        },
    },
    ir::{
        classes::SymAggregateStyle,
        types::{SymGenericTerm, SymPerm, SymPermKind, SymPlace, SymTy, SymTyKind, SymTyName},
    },
};

pub(crate) async fn term_is_copy<'db>(env: &Env<'db>, term: SymGenericTerm<'db>) -> Errors<bool> {
    match term {
        SymGenericTerm::Type(sym_ty) => ty_is_copy(env, sym_ty).await,
        SymGenericTerm::Perm(sym_perm) => perm_is_copy(env, sym_perm).await,
        SymGenericTerm::Place(sym_place) => panic!("term_is invoked on place: {sym_place:?}"),
        SymGenericTerm::Error(reported) => Err(reported),
    }
}

#[boxed_async_fn]
async fn ty_is_copy<'db>(env: &Env<'db>, ty: SymTy<'db>) -> Errors<bool> {
    let db = env.db();
    match *ty.kind(db) {
        SymTyKind::Perm(sym_perm, sym_ty) => {
            Ok(application_is_copy(env, sym_perm.into(), sym_ty.into()).await?)
        }
        SymTyKind::Infer(infer) => Ok(test_infer_is(env, infer, Predicate::Copy).await),
        SymTyKind::Var(var) => Ok(test_var_is(env, var, Predicate::Copy)),
        SymTyKind::Never => Ok(false),
        SymTyKind::Error(reported) => Err(reported),
        SymTyKind::Named(sym_ty_name, ref generics) => match sym_ty_name {
            SymTyName::Primitive(_) => Ok(true),
            SymTyName::Aggregate(sym_aggregate) => match sym_aggregate.style(db) {
                SymAggregateStyle::Struct => {
                    for_all(generics, async |&generic| term_is_copy(env, generic).await).await
                }
                SymAggregateStyle::Class => Ok(false),
            },
            SymTyName::Future => Ok(false),
            SymTyName::Tuple { arity: _ } => {
                for_all(generics, async |&generic| term_is_copy(env, generic).await).await
            }
        },
    }
}

async fn application_is_copy<'db>(
    env: &Env<'db>,
    lhs: SymGenericTerm<'db>,
    rhs: SymGenericTerm<'db>,
) -> Errors<bool> {
    either(term_is_copy(env, lhs), term_is_copy(env, rhs)).await
}

#[boxed_async_fn]
pub(crate) async fn perm_is_copy<'db>(env: &Env<'db>, perm: SymPerm<'db>) -> Errors<bool> {
    let db = env.db();
    match *perm.kind(db) {
        SymPermKind::Error(reported) => Err(reported),
        SymPermKind::My => Ok(false),
        SymPermKind::Our | SymPermKind::Shared(_) => Ok(true),
        SymPermKind::Leased(ref places) => places_are_copy(env, places).await,
        SymPermKind::Apply(lhs, rhs) => Ok(application_is_copy(env, lhs.into(), rhs.into()).await?),
        SymPermKind::Var(var) => Ok(test_var_is(env, var, Predicate::Copy)),
        SymPermKind::Infer(infer) => Ok(test_infer_is(env, infer, Predicate::Copy).await),
    }
}

#[boxed_async_fn]
async fn places_are_copy<'db>(env: &Env<'db>, places: &[SymPlace<'db>]) -> Errors<bool> {
    for_all(places, async |&place| place_is_copy(env, place).await).await
}

pub(crate) async fn place_is_copy<'db>(env: &Env<'db>, place: SymPlace<'db>) -> Errors<bool> {
    let ty = place.place_ty(env).await;
    ty_is_copy(env, ty).await
}
