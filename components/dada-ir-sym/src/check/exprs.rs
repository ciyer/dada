use std::panic::Location;

use crate::{
    check::{
        env::Env,
        member_lookup::MemberLookup,
        scope::{NameResolution, NameResolutionSym},
        scope_tree::ScopeTreeNode,
    },
    ir::{
        binder::Binder,
        classes::SymAggregate,
        exprs::{
            SymBinaryOp, SymByteLiteral, SymByteLiteralData, SymExpr, SymExprKind, SymLiteral,
            SymMatchArm, SymPlaceExpr, SymPlaceExprKind,
        },
        functions::{SymFunction, SymInputOutput},
        types::{SymGenericKind, SymGenericTerm, SymTy, SymTyKind, SymTyName},
        variables::{FromVar, SymVariable},
    },
    prelude::CheckedSignature,
    well_known,
};
use dada_ir_ast::{
    ast::{
        AstBinaryOp, AstExpr, AstExprKind, AstGenericTerm, Identifier, LiteralKind, PermissionOp,
        SpanVec, SpannedBinaryOp, SpannedIdentifier, UnaryOp,
    },
    diagnostic::{Diagnostic, Err, Level, Reported},
    span::{Span, Spanned},
};
use dada_parser::prelude::*;
use dada_util::{FromImpls, boxed_async_fn};
use serde::Serialize;

use super::{
    CheckExprInEnv, CheckTyInEnv,
    debug::TaskDescription,
    live_places::LivePlaces,
    report::{
        AwaitNonFuture, BadSubtypeError, InvalidAssignmentType, InvalidReturnValue,
        NumericTypeExpected, OperatorArgumentsMustHaveSameType, OperatorRequiresNumericType,
        WhereClauseError,
    },
    temporaries::Temporary,
};

#[derive(Clone, Serialize)]
pub(crate) struct ExprResult<'db> {
    /// List of [`Temporary`][] variables created by this expression.
    pub temporaries: Vec<Temporary<'db>>,

    /// Span of the expression
    pub span: Span<'db>,

    /// The primary result from translating an expression.
    /// Note that an ast-expr can result in many kinds of things.
    pub kind: ExprResultKind<'db>,
}

#[derive(Clone, Debug, FromImpls, Serialize)]
pub(crate) enum ExprResultKind<'db> {
    /// An expression identifying a place in memory (e.g., a local variable).
    PlaceExpr(SymPlaceExpr<'db>),

    /// An expression that produces a value.
    Expr(SymExpr<'db>),

    /// A partially completed method call.
    #[no_from_impl]
    Method {
        self_expr: SymExpr<'db>,
        id_span: Span<'db>,
        function: SymFunction<'db>,
        generics: Option<SpanVec<'db, AstGenericTerm<'db>>>,
    },

    /// Some kind of name resoluton that cannot be represented by as an expression.
    Other(NameResolution<'db>),
}

impl<'db> CheckExprInEnv<'db> for AstExpr<'db> {
    type Output = ExprResult<'db>;

    async fn check_in_env(&self, env: &mut Env<'db>, live_after: LivePlaces) -> Self::Output {
        check_expr(self, env, live_after).await
    }
}

#[boxed_async_fn]
async fn check_expr<'db>(
    expr: &AstExpr<'db>,
    env: &mut Env<'db>,
    live_after: LivePlaces,
) -> ExprResult<'db> {
    env.indent("check_expr", &[expr], async |env| {
        let db = env.db();
        let expr_span = expr.span;

        match &*expr.kind {
            AstExprKind::Literal(literal) => match literal.kind(db) {
                LiteralKind::Integer => {
                    let ty = env.fresh_ty_inference_var(expr_span);
                    let bits = match str::parse(literal.text(db)) {
                        Ok(v) => v,
                        Err(e) => panic!("error: {e:?}"),
                    };
                    let sym_expr = SymExpr::new(
                        db,
                        expr_span,
                        ty,
                        SymExprKind::Primitive(SymLiteral::Integral { bits }),
                    );
                    env.spawn_require_my_numeric_type(
                        LivePlaces::fixme(),
                        ty,
                        &NumericTypeExpected::new(sym_expr, ty),
                    );
                    ExprResult {
                        temporaries: vec![],
                        span: expr_span,
                        kind: sym_expr.into(),
                    }
                }

                LiteralKind::String => {
                    // Generate `String.literal(b"...", length)`

                    // Generate `b"..."`
                    let bytes = literal.text(db).as_bytes();
                    let byte_literal_expr = {
                        let pointer_struct = match well_known::pointer_struct(db) {
                            Ok(v) => v,
                            Err(reported) => return ExprResult::err(db, reported),
                        };
                        let data = SymByteLiteralData::new(db, bytes);
                        let byte_literal = SymByteLiteral::new(db, expr_span, data);
                        SymExpr::new(
                            db,
                            expr_span,
                            SymTy::named(db, pointer_struct.into(), vec![SymTy::u8(db).into()]),
                            SymExprKind::ByteLiteral(byte_literal),
                        )
                    };

                    // Generate `length`
                    let len_literal_expr = {
                        let value = bytes.len() as u64;
                        SymExpr::new(
                            db,
                            expr_span,
                            SymTy::u32(db),
                            SymExprKind::Primitive(SymLiteral::Integral { bits: value }),
                        )
                    };

                    // Generate and return `String.literal(b"...", length)`
                    let mut temporaries = vec![];
                    let ctor_call_expr = {
                        let literal_fn = match well_known::string_literal_fn(db) {
                            Ok(v) => v,
                            Err(reported) => return ExprResult::err(db, reported),
                        };
                        SymExpr::new(
                            db,
                            expr_span,
                            SymTy::string(db),
                            SymExprKind::Call {
                                function: literal_fn,
                                substitution: vec![],
                                arg_temps: vec![
                                    byte_literal_expr.into_temporary_var(db, &mut temporaries),
                                    len_literal_expr.into_temporary_var(db, &mut temporaries),
                                ],
                            },
                        )
                    };
                    ExprResult {
                        temporaries,
                        span: expr_span,
                        kind: ctor_call_expr.into(),
                    }
                }

                LiteralKind::Boolean => {
                    let bits = match &literal.text(db)[..] {
                        "true" => 1,
                        "false" => 0,
                        t => panic!("unrecognized boolean literal {t:?}"),
                    };
                    ExprResult {
                        temporaries: vec![],
                        span: expr_span,
                        kind: SymExpr::new(
                            db,
                            expr_span,
                            SymTy::boolean(db),
                            SymExprKind::Primitive(SymLiteral::Integral { bits }),
                        )
                        .into(),
                    }
                }
            },

            AstExprKind::Tuple(span_vec) => {
                let mut temporaries = vec![];
                let mut exprs = vec![];
                for element in &span_vec.values {
                    exprs.push(
                        element
                            .check_in_env(env, LivePlaces::fixme())
                            .await
                            .into_expr(env, &mut temporaries),
                    );
                }

                let ty = SymTy::new(
                    db,
                    SymTyKind::Named(
                        SymTyName::Tuple { arity: exprs.len() },
                        exprs.iter().map(|e| e.ty(db).into()).collect(),
                    ),
                );

                ExprResult {
                    temporaries,
                    span: expr_span,
                    kind: ExprResultKind::Expr(SymExpr::new(
                        db,
                        expr_span,
                        ty,
                        SymExprKind::Tuple(exprs),
                    )),
                }
            }

            AstExprKind::BinaryOp(span_op, lhs, rhs) => {
                let span_op: SpannedBinaryOp<'db> = *span_op;
                match span_op.op {
                    AstBinaryOp::Add | AstBinaryOp::Sub | AstBinaryOp::Mul | AstBinaryOp::Div => {
                        let mut temporaries: Vec<Temporary<'db>> = vec![];
                        let lhs: SymExpr<'db> = lhs
                            .check_in_env(env, LivePlaces::fixme())
                            .await
                            .into_expr(env, &mut temporaries);
                        let rhs: SymExpr<'db> = rhs
                            .check_in_env(env, LivePlaces::fixme())
                            .await
                            .into_expr(env, &mut temporaries);

                        // For now, let's do a dumb rule that operands must be
                        // of the same primitive (and scalar) type.

                        env.spawn_require_numeric_type(
                            lhs.ty(db),
                            &OperatorRequiresNumericType::new(span_op, lhs),
                        );
                        env.spawn_require_numeric_type(
                            rhs.ty(db),
                            &OperatorRequiresNumericType::new(span_op, rhs),
                        );
                        env.spawn_if_not_never(&[lhs.ty(db), rhs.ty(db)], async move |env| {
                            env.spawn_require_equal_types(
                                live_after,
                                lhs.ty(db),
                                rhs.ty(db),
                                &OperatorArgumentsMustHaveSameType::new(span_op, lhs, rhs),
                            );
                        });

                        // What type do we want these operators to have?
                        // For now I'll just take the LHS, but that seems
                        // wrong if e.g. one side is `!`, then we probably
                        // want `!`, right?

                        ExprResult::from_expr(
                            env.db(),
                            SymExpr::new(
                                db,
                                expr_span,
                                lhs.ty(db),
                                SymExprKind::BinaryOp(
                                    SymBinaryOp::try_from(span_op.op).expect("invalid binary op"),
                                    lhs,
                                    rhs,
                                ),
                            ),
                            temporaries,
                        )
                    }

                    AstBinaryOp::AndAnd => {
                        let mut temporaries: Vec<Temporary<'db>> = vec![];
                        let lhs: SymExpr<'db> = lhs
                            .check_in_env(env, LivePlaces::fixme())
                            .await
                            .into_expr(env, &mut temporaries);
                        let rhs: SymExpr<'db> = rhs
                            .check_in_env(env, live_after)
                            .await
                            .into_expr(env, &mut temporaries);
                        env.require_expr_has_bool_ty(LivePlaces::fixme(), lhs);
                        env.require_expr_has_bool_ty(live_after, rhs);

                        // construct an expression like
                        // if lhs { if rhs { true } else { false } } else { false }
                        ExprResult::from_expr(
                            env.db(),
                            SymExpr::if_then_else(
                                db,
                                expr_span,
                                lhs,
                                SymExpr::if_then_else(
                                    db,
                                    expr_span,
                                    rhs,
                                    SymExpr::true_literal(db, expr_span),
                                    SymExpr::false_literal(db, expr_span),
                                ),
                                SymExpr::false_literal(db, expr_span),
                            ),
                            temporaries,
                        )
                    }

                    AstBinaryOp::OrOr => {
                        let mut temporaries: Vec<Temporary<'db>> = vec![];
                        let lhs: SymExpr<'db> = lhs
                            .check_in_env(env, LivePlaces::fixme())
                            .await
                            .into_expr(env, &mut temporaries);
                        let rhs: SymExpr<'db> = rhs
                            .check_in_env(env, live_after)
                            .await
                            .into_expr(env, &mut temporaries);

                        env.require_expr_has_bool_ty(LivePlaces::fixme(), lhs);
                        env.require_expr_has_bool_ty(live_after, rhs);

                        // construct an expression like
                        // if lhs { true } else { if rhs { true } else { false } }
                        ExprResult::from_expr(
                            env.db(),
                            SymExpr::if_then_else(
                                db,
                                expr_span,
                                lhs,
                                SymExpr::true_literal(db, expr_span),
                                SymExpr::if_then_else(
                                    db,
                                    expr_span,
                                    rhs,
                                    SymExpr::true_literal(db, expr_span),
                                    SymExpr::false_literal(db, expr_span),
                                ),
                            ),
                            temporaries,
                        )
                    }

                    AstBinaryOp::GreaterThan
                    | AstBinaryOp::LessThan
                    | AstBinaryOp::GreaterEqual
                    | AstBinaryOp::LessEqual
                    | AstBinaryOp::EqualEqual => {
                        let mut temporaries: Vec<Temporary<'db>> = vec![];
                        let lhs: SymExpr<'db> = lhs
                            .check_in_env(env, LivePlaces::fixme())
                            .await
                            .into_expr(env, &mut temporaries);
                        let rhs: SymExpr<'db> = rhs
                            .check_in_env(env, LivePlaces::fixme())
                            .await
                            .into_expr(env, &mut temporaries);

                        // For now, let's do a dumb rule that operands must be
                        // of the same primitive (and scalar) type.

                        env.spawn_require_numeric_type(
                            lhs.ty(db),
                            &OperatorRequiresNumericType::new(span_op, lhs),
                        );
                        env.spawn_require_numeric_type(
                            rhs.ty(db),
                            &OperatorRequiresNumericType::new(span_op, rhs),
                        );
                        env.spawn_if_not_never(&[lhs.ty(db), rhs.ty(db)], async move |env| {
                            env.spawn_require_equal_types(
                                LivePlaces::fixme(),
                                lhs.ty(db),
                                rhs.ty(db),
                                &OperatorArgumentsMustHaveSameType::new(span_op, lhs, rhs),
                            );
                        });

                        // What type do we want these operators to have?
                        // For now I'll just take the LHS, but that seems
                        // wrong if e.g. one side is `!`, then we probably
                        // want `!`, right?

                        ExprResult::from_expr(
                            env.db(),
                            SymExpr::new(
                                db,
                                expr_span,
                                SymTy::boolean(db),
                                SymExprKind::BinaryOp(
                                    SymBinaryOp::try_from(span_op.op).expect("invalid binary op"),
                                    lhs,
                                    rhs,
                                ),
                            ),
                            temporaries,
                        )
                    }

                    AstBinaryOp::Assign => {
                        let mut temporaries: Vec<Temporary<'db>> = vec![];
                        let place: SymPlaceExpr<'db> = lhs
                            .check_in_env(env, LivePlaces::fixme())
                            .await
                            .into_place_expr(env, &mut temporaries);
                        let value: SymExpr<'db> = rhs
                            .check_in_env(env, LivePlaces::fixme())
                            .await
                            .into_expr(env, &mut temporaries);

                        // For now, let's do a dumb rule that operands must be
                        // of the same primitive (and scalar) type.

                        env.spawn_require_assignable_type(
                            LivePlaces::fixme(),
                            value.ty(db),
                            place.ty(db),
                            &InvalidAssignmentType::new(place, value),
                        );

                        ExprResult::from_expr(
                            env.db(),
                            SymExpr::new(
                                db,
                                expr_span,
                                SymTy::unit(db),
                                SymExprKind::Assign { place, value },
                            ),
                            temporaries,
                        )
                    }
                }
            }

            AstExprKind::Id(SpannedIdentifier { span: id_span, id }) => {
                match env.scope.resolve_name(db, *id, *id_span) {
                    Err(reported) => ExprResult::err(db, reported),
                    Ok(res) => ExprResult::from_name_resolution(env, res, expr_span).await,
                }
            }

            AstExprKind::DotId(owner, id) => {
                let mut owner_result = owner.check_in_env(env, live_after).await;
                match owner_result.kind {
                    ExprResultKind::PlaceExpr(_) | ExprResultKind::Expr(_) => {
                        MemberLookup::new(env)
                            .lookup_member(owner_result, *id)
                            .await
                    }

                    ExprResultKind::Other(name_resolution) => {
                        match name_resolution.resolve_relative_id(db, *id) {
                            // Got an error? Bail out.
                            Err(reported) => ExprResult::err(db, reported),

                            // Found something with lexical resolution? Continue.
                            Ok(Ok(r)) => ExprResult::from_name_resolution(env, r, expr_span).await,

                            // Otherwise, try type-dependent lookup.
                            Ok(Err(name_resolution)) => {
                                owner_result.kind = name_resolution.into();
                                MemberLookup::new(env)
                                    .lookup_member(owner_result, *id)
                                    .await
                            }
                        }
                    }

                    ExprResultKind::Method {
                        self_expr: owner,
                        function: method,
                        ..
                    } => ExprResult::err(
                        db,
                        report_missing_call_to_method(db, owner.span(db), method),
                    ),
                }
            }

            AstExprKind::SquareBracketOp(owner, square_bracket_args) => {
                let owner_result = owner.check_in_env(env, LivePlaces::fixme()).await;
                match owner_result.kind {
                    ExprResultKind::Method {
                        self_expr: owner,
                        function: method,
                        generics: None,
                        id_span,
                    } => {
                        let ast_terms = square_bracket_args.parse_as_generics(db);

                        ExprResult {
                            kind: ExprResultKind::Method {
                                self_expr: owner,
                                function: method,
                                generics: Some(ast_terms),
                                id_span,
                            },
                            ..owner_result
                        }
                    }

                    ExprResultKind::PlaceExpr(_) | ExprResultKind::Expr(_) => ExprResult::err(
                        db,
                        report_not_implemented(db, expr_span, "indexing expressions"),
                    ),

                    // We see something like `foo.bar[][]` where `bar` is a method.
                    // The only correct thing here would be `foo.bar[]()[]`, i.e., call the method and then index.
                    // We give an error under that assumption.
                    // It seems likely we can do a better job.
                    ExprResultKind::Method {
                        self_expr: owner,
                        function: method,
                        generics: Some(_),
                        ..
                    } => ExprResult::err(
                        db,
                        report_missing_call_to_method(db, owner.span(db), method),
                    ),

                    ExprResultKind::Other(name_resolution) => {
                        let generics = square_bracket_args.parse_as_generics(db);
                        match name_resolution
                            .resolve_relative_generic_args(env, &generics)
                            .await
                        {
                            Ok(name_resolution) => ExprResult {
                                temporaries: owner_result.temporaries,
                                span: expr_span,
                                kind: name_resolution.into(),
                            },
                            Err(r) => ExprResult::err(db, r),
                        }
                    }
                }
            }

            AstExprKind::ParenthesisOp(owner, ast_args) => {
                let owner_result = owner.check_in_env(env, live_after).await;
                match owner_result {
                    ExprResult {
                        temporaries,
                        span: expr_span,
                        kind:
                            ExprResultKind::Method {
                                self_expr,
                                id_span,
                                function,
                                generics,
                            },
                    } => {
                        check_method_call(
                            env,
                            live_after,
                            id_span,
                            expr_span,
                            function,
                            Some(self_expr),
                            ast_args,
                            generics,
                            temporaries,
                        )
                        .await
                    }

                    ExprResult {
                        temporaries,
                        span: function_span,
                        kind:
                            ExprResultKind::Other(NameResolution {
                                generics,
                                sym: NameResolutionSym::SymFunction(sym),
                                ..
                            }),
                    } => {
                        check_function_call(
                            env,
                            live_after,
                            function_span,
                            expr_span,
                            sym,
                            ast_args,
                            generics,
                            temporaries,
                        )
                        .await
                    }

                    // Calling a class like `Class(a, b)`: convert to
                    // `Class.new(a, b)`.
                    ExprResult {
                        temporaries,
                        span: class_span,
                        kind:
                            ExprResultKind::Other(
                                name_resolution @ NameResolution {
                                    sym: NameResolutionSym::SymAggregate(class_sym),
                                    ..
                                },
                            ),
                    } => {
                        check_class_call(
                            env,
                            live_after,
                            class_span,
                            expr_span,
                            name_resolution,
                            class_sym,
                            ast_args,
                            temporaries,
                        )
                        .await
                    }

                    ExprResult {
                        span: owner_span, ..
                    } => {
                        // FIXME: we probably want to support functions and function typed values?
                        ExprResult::err(db, report_not_callable(db, owner_span))
                    }
                }
            }

            AstExprKind::Constructor(_ast_path, _span_vec) => todo!(),
            AstExprKind::Return(ast_expr) => {
                let mut temporaries = vec![];

                let return_expr = if let Some(ast_expr) = ast_expr {
                    ast_expr
                        .check_in_env(env, LivePlaces::none(env))
                        .await
                        .into_expr(env, &mut temporaries)
                } else {
                    // the default is `return ()`
                    SymExpr::new(db, expr_span, SymTy::unit(db), SymExprKind::Tuple(vec![]))
                };

                let Some(expected_return_ty) = env.return_ty else {
                    return ExprResult::err(
                        db,
                        env.report(
                            Diagnostic::error(
                                db,
                                expr_span,
                                "unexpected `return` statement".to_string(),
                            )
                            .label(
                                db,
                                Level::Error,
                                expr_span,
                                "I did not expect to see a `return` statement here".to_string(),
                            ),
                        ),
                    );
                };

                env.spawn_require_assignable_type(
                    LivePlaces::none(env),
                    return_expr.ty(db),
                    expected_return_ty,
                    &InvalidReturnValue::new(return_expr, expected_return_ty),
                );

                ExprResult {
                    temporaries,
                    span: expr_span,
                    kind: SymExpr::new(
                        db,
                        expr_span,
                        SymTy::never(db),
                        SymExprKind::Return(return_expr),
                    )
                    .into(),
                }
            }

            AstExprKind::Await {
                future,
                await_keyword,
            } => {
                let await_span = *await_keyword;

                let mut temporaries = vec![];

                let future_expr = future
                    .check_in_env(env, live_after)
                    .await
                    .into_expr(env, &mut temporaries);
                let future_ty = future_expr.ty(db);

                let awaited_ty = env.fresh_ty_inference_var(await_span);

                env.spawn_require_future_type(
                    live_after,
                    future_ty,
                    awaited_ty,
                    &AwaitNonFuture::new(await_span, future_expr),
                );

                ExprResult {
                    temporaries,
                    span: expr_span,
                    kind: SymExpr::new(
                        db,
                        expr_span,
                        awaited_ty,
                        SymExprKind::Await {
                            future: future_expr,
                            await_keyword: await_span,
                        },
                    )
                    .into(),
                }
            }
            AstExprKind::UnaryOp(spanned_unary_op, ast_expr) => match spanned_unary_op.op {
                UnaryOp::Not => {
                    let mut temporaries = vec![];
                    let operand = ast_expr
                        .check_in_env(env, live_after)
                        .await
                        .into_expr(env, &mut temporaries);
                    env.require_expr_has_bool_ty(live_after, operand);

                    ExprResult {
                        temporaries,
                        span: expr_span,
                        kind: SymExpr::new(
                            db,
                            expr_span,
                            SymTy::boolean(db),
                            SymExprKind::Not {
                                operand,
                                op_span: spanned_unary_op.span,
                            },
                        )
                        .into(),
                    }
                }
                UnaryOp::Negate => todo!(),
            },

            AstExprKind::Block(ast_block) => ExprResult {
                temporaries: vec![],
                span: expr_span,
                kind: ast_block.check_in_env(env, live_after).await.into(),
            },

            AstExprKind::If(ast_arms) => {
                let mut arms = vec![];
                let mut has_else = false;
                for arm in ast_arms {
                    let condition = if let Some(c) = &arm.condition {
                        let expr = c
                            .check_in_env(env, LivePlaces::fixme())
                            .await
                            .into_expr_with_enclosed_temporaries(env);
                        env.require_expr_has_bool_ty(LivePlaces::fixme(), expr);
                        Some(expr)
                    } else {
                        has_else = true;
                        None
                    };

                    let body = arm.result.check_in_env(env, live_after).await;

                    arms.push(SymMatchArm { condition, body });
                }

                let if_ty = if !has_else {
                    SymTy::unit(db)
                } else {
                    env.fresh_ty_inference_var(expr_span)
                };

                for arm in &arms {
                    env.spawn_require_assignable_type(
                        live_after,
                        arm.body.ty(db),
                        if_ty,
                        &BadSubtypeError::new(arm.body.span(db), arm.body.ty(db), if_ty),
                    );
                }

                ExprResult {
                    temporaries: vec![],
                    span: expr_span,
                    kind: SymExpr::new(db, expr_span, if_ty, SymExprKind::Match { arms }).into(),
                }
            }

            AstExprKind::PermissionOp { value, op } => {
                let mut temporaries = vec![];
                let value_result = value.check_in_env(env, live_after).await;
                let place_expr = value_result.into_place_expr(env, &mut temporaries);
                let sym_place = place_expr.into_sym_place(db);
                ExprResult {
                    temporaries,
                    span: expr_span,
                    kind: SymExpr::new(
                        db,
                        expr_span,
                        match op {
                            PermissionOp::Mutate => place_expr.ty(db).mutable(db, sym_place),
                            PermissionOp::Reference => place_expr.ty(db).referenced(db, sym_place),
                            PermissionOp::Give => place_expr.ty(db),
                        },
                        SymExprKind::PermissionOp(*op, place_expr),
                    )
                    .into(),
                }
            }
        }
    })
    .await
}

#[boxed_async_fn]
async fn check_class_call<'db>(
    env: &mut Env<'db>,
    live_after: LivePlaces,
    class_span: Span<'db>,
    expr_span: Span<'db>,
    name_resolution: NameResolution<'db>,
    class_sym: SymAggregate<'db>,
    ast_args: &SpanVec<'db, AstExpr<'db>>,
    temporaries: Vec<Temporary<'db>>,
) -> ExprResult<'db> {
    let db = env.db();

    let new_ident = SpannedIdentifier {
        span: class_span,
        id: Identifier::new_ident(db),
    };

    let (generics, new_function) = match name_resolution.resolve_relative_id(db, new_ident) {
        Ok(Ok(NameResolution {
            generics,
            sym: NameResolutionSym::SymFunction(m),
        })) => (generics, m),
        Ok(r) => return ExprResult::err(db, report_no_new_method(db, class_span, class_sym, r)),
        Err(reported) => return ExprResult::err(db, reported),
    };

    check_function_call(
        env,
        live_after,
        class_span,
        expr_span,
        new_function,
        ast_args,
        generics,
        temporaries,
    )
    .await
}

fn report_no_new_method<'db>(
    db: &'db dyn crate::Db,
    class_span: Span<'db>,
    class_sym: SymAggregate<'db>,
    resolution: Result<NameResolution<'db>, NameResolution<'db>>,
) -> Reported {
    let mut diag = Diagnostic::error(
        db,
        class_span,
        format!("the class `{class_sym}` has no `new` method"),
    )
    .label(
        db,
        Level::Error,
        class_span,
        format!("I could not find a `new` method on the class `{class_sym}`"),
    );

    match resolution {
        Ok(name_resolution) => {
            diag = diag.child(
                Diagnostic::new(
                    db,
                    Level::Note,
                    class_sym.name_span(db),
                    "calling a class is equivalent to calling `new`, but `new` is not a method"
                        .to_string(),
                )
                .label(
                    db,
                    Level::Note,
                    name_resolution.span(db).unwrap(),
                    format!(
                        "I found a class member named `new` but it is {}, not a method",
                        name_resolution.categorize(db)
                    ),
                ),
            );
        }
        Err(_) => {
            diag = diag.child(
                Diagnostic::new(
                    db,
                    Level::Note,
                    class_sym.name_span(db),
                    format!(
                        "calling a class is equivalent to calling `new`, but `{class_sym}` does not define a `new` method"
                    ),
                )
                .label(
                    db,
                    Level::Note,
                    class_sym.name_span(db),
                    "I could not find any class member named `new`".to_string(),
                ),
            );
        }
    }

    diag.report(db)
}

#[boxed_async_fn]
async fn check_function_call<'db>(
    env: &mut Env<'db>,
    live_after: LivePlaces,
    function_span: Span<'db>,
    expr_span: Span<'db>,
    function: SymFunction<'db>,
    ast_args: &SpanVec<'db, AstExpr<'db>>,
    generics: Vec<SymGenericTerm<'db>>,
    temporaries: Vec<Temporary<'db>>,
) -> ExprResult<'db> {
    let db = env.db();

    env.log("check_function_call", &[&function]);
    env.log("generics", &[&generics]);

    // Get the signature.
    let signature = match function.checked_signature(db) {
        Ok(signature) => signature,
        Err(reported) => {
            for ast_arg in ast_args {
                let _ = ast_arg.check_in_env(env, LivePlaces::fixme()).await;
            }
            return ExprResult::err(db, reported);
        }
    };
    let input_output = signature.input_output(db);

    env.log("input_output", &[&input_output]);

    // Create inference variables for any generic arguments not provided.
    let expected_generics = function.transitive_generic_parameters(db);
    env.log("expected_generics", &[&expected_generics]);
    let mut substitution = generics.clone();
    substitution.extend(
        expected_generics[generics.len()..]
            .iter()
            .map(|&var| env.fresh_inference_var_term(var.kind(db), function_span)),
    );

    check_call_common(
        env,
        live_after,
        function,
        expr_span,
        function_span,
        input_output,
        substitution,
        ast_args,
        None,
        temporaries,
    )
    .await
}

/// Check a call like `a.b()` where `b` is a method.
/// These are somewhat different than calls like `b(a)` because of how
/// type arguments are handled.
#[allow(clippy::too_many_arguments)]
#[boxed_async_fn]
async fn check_method_call<'db>(
    env: &mut Env<'db>,
    live_after: LivePlaces,
    id_span: Span<'db>,
    expr_span: Span<'db>,
    function: SymFunction<'db>,
    self_expr: Option<SymExpr<'db>>,
    ast_args: &[AstExpr<'db>],
    generics: Option<SpanVec<'db, AstGenericTerm<'db>>>,
    temporaries: Vec<Temporary<'db>>,
) -> ExprResult<'db> {
    let db = env.db();

    // Get the signature.
    let signature = match function.checked_signature(db) {
        Ok(signature) => signature,
        Err(reported) => {
            for &generic in generics.iter().flatten() {
                let _ = generic.check_in_env(env).await;
            }
            for ast_arg in ast_args {
                let _ = ast_arg.check_in_env(env, LivePlaces::fixme()).await;
            }
            return ExprResult::err(db, reported);
        }
    };
    let input_output = signature.input_output(db);

    // Prepare the substitution for the function.
    let substitution = match generics {
        None => {
            // Easy case: nothing provided by user, just create inference variables for everything.
            env.existential_substitution(id_span, &input_output.variables)
        }

        Some(generics) => {
            // Harder case: user provided generics. Given that we are parsing a call like `a.b()`,
            // then generic arguments would be `a.b[x, y]()` and therefore correspond to the generics declared on
            // the function itself. But the `input_output` binder can contain additional parameters from
            // the class or surrounding scope. Those add'l parameters are instantiated with inference
            // variables-- and then the user-provided generics are added afterwards.

            // Create existential substitution for any other generics.
            let function_generics = &signature.symbols(db).generic_variables;
            assert!(input_output.variables.ends_with(function_generics));
            let outer_variables =
                &input_output.variables[0..input_output.variables.len() - function_generics.len()];
            let mut substitution: Vec<SymGenericTerm<'_>> =
                env.existential_substitution(id_span, outer_variables);

            // Check the user gave the expected number of arguments.
            if function_generics.len() != generics.len() {
                return ExprResult::err(
                    db,
                    env.report(
                        Diagnostic::error(
                            db,
                            id_span,
                            format!(
                                "expected {expected} generic arguments, but found {found}",
                                expected = function_generics.len(),
                                found = generics.len()
                            ),
                        )
                        .label(
                            db,
                            Level::Error,
                            id_span,
                            format!(
                                "{found} generic arguments were provided",
                                found = generics.len()
                            ),
                        )
                        .label(
                            db,
                            Level::Error,
                            function.name_span(db),
                            format!(
                                "the function `{name}` is declared with {expected} generic arguments",
                                name = function.name(db),
                                expected = function_generics.len(),
                            ),
                        ),
                    ),
                );
            }

            // Convert each generic to a `SymGenericTerm` and check it has the correct kind.
            // If everything looks good, add it to the substitution.
            for (&ast_generic_term, &var) in generics.iter().zip(function_generics.iter()) {
                let generic_term = ast_generic_term.check_in_env(env).await;
                if !generic_term.has_kind(db, var.kind(db)) {
                    return ExprResult::err(
                        db,
                        env.report(
                            Diagnostic::error(
                                db,
                                ast_generic_term.span(db),
                                format!(
                                    "expected `{expected_kind}`, found `{found_kind}`",
                                    expected_kind = var.kind(db),
                                    found_kind = generic_term.kind().unwrap(),
                                ),
                            )
                            .label(
                                db,
                                Level::Error,
                                id_span,
                                format!(
                                    "this is a `{found_kind}`",
                                    found_kind = generic_term.kind().unwrap(),
                                ),
                            )
                            .label(
                                db,
                                Level::Info,
                                var.span(db),
                                format!(
                                    "I expected to find a `{expected_kind}`",
                                    expected_kind = var.kind(db),
                                ),
                            ),
                        ),
                    );
                }
                substitution.push(generic_term);
            }

            substitution
        }
    };

    check_call_common(
        env,
        live_after,
        function,
        expr_span,
        id_span,
        input_output,
        substitution,
        ast_args,
        self_expr,
        temporaries,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
#[boxed_async_fn]
async fn check_call_common<'db>(
    env: &mut Env<'db>,
    _live_after: LivePlaces,
    function: SymFunction<'db>,
    expr_span: Span<'db>,
    callee_span: Span<'db>,
    input_output: &Binder<'db, Binder<'db, SymInputOutput<'db>>>,
    substitution: Vec<SymGenericTerm<'db>>,
    ast_args: &[AstExpr<'db>],
    self_expr: Option<SymExpr<'db>>,
    mut temporaries: Vec<Temporary<'db>>,
) -> ExprResult<'db> {
    let db = env.db();

    env.log("check_call_common", &[]);
    env.log("substitution", &[&substitution]);

    // Instantiate the input-output with the substitution.
    let input_output = input_output.substitute(db, &substitution);

    // Check the arity of the actual arguments.
    let self_args: usize = self_expr.is_some() as usize;
    let expected_inputs = input_output.bound_value.input_tys.len();
    let found_inputs = self_args + ast_args.len();
    if found_inputs != expected_inputs {
        let function_name = function.name(db);
        return ExprResult::err(
            db,
            env.report(
                Diagnostic::error(
                    db,
                    callee_span,
                    format!("expected {expected_inputs} arguments, found {found_inputs}"),
                )
                .label(
                    db,
                    Level::Error,
                    callee_span,
                    format!("I expected `{function_name}` to take {expected_inputs} arguments but I found {found_inputs}",),
                )
                .label(
                    db,
                    Level::Info,
                    function.name_span(db),
                    format!("`{function_name}` defined here"),
                )
            ),
        );
    }

    // Create the temporaries that will hold the values for each argument.
    let arg_temp_span = |i: usize| {
        if i < self_args {
            self_expr.unwrap().span(db)
        } else {
            ast_args
                .get(i - self_args)
                .map(|a| a.span)
                .unwrap_or(callee_span)
        }
    };
    let arg_temp_symbols = (0..expected_inputs)
        .map(|i| SymVariable::new(db, SymGenericKind::Place, None, arg_temp_span(i)))
        .collect::<Vec<_>>();
    let arg_temp_terms = arg_temp_symbols
        .iter()
        .map(|&sym| SymGenericTerm::var(db, sym))
        .collect::<Vec<_>>();

    // Instantiate the final level of binding with those temporaries
    let input_output: SymInputOutput<'_> = input_output.substitute(db, &arg_temp_terms);

    env.log("arg_temp_symbols", &[&arg_temp_symbols]);
    env.log("arg_temp_terms", &[&arg_temp_terms]);
    env.log("input_output", &[&input_output]);

    // Function to type check a single argument and check it has the correct type.
    let check_arg = async |i: usize| -> ExprResult<'db> {
        let mut env = env.fork(|log| log.spawn(Location::caller(), TaskDescription::CheckArg(i)));
        let mut arg_temporaries = vec![];
        let expr = if i < self_args {
            self_expr.unwrap()
        } else {
            let ast_arg = &ast_args[i - self_args];
            ast_arg
                .check_in_env(&mut env, LivePlaces::fixme())
                .await
                .into_expr(&mut env, &mut arg_temporaries)
        };
        env.spawn_require_assignable_type(
            LivePlaces::fixme(),
            expr.ty(db),
            input_output.input_tys[i],
            &BadSubtypeError::new(expr.span(db), expr.ty(db), input_output.input_tys[i]),
        );
        ExprResult::from_expr(env.db(), expr, arg_temporaries)
    };

    // Spawn out work to check the predicates.
    for where_clause in input_output.where_clauses {
        env.spawn_require_where_clause(
            where_clause,
            &WhereClauseError::new(callee_span, where_clause),
        );
    }

    // Type check the arguments; these can proceed concurrently.
    let mut arg_exprs = vec![];
    arg_exprs.extend(self_expr);
    for arg_result in futures::future::join_all((0..found_inputs).map(check_arg)).await {
        arg_exprs.push(arg_result.into_expr(env, &mut temporaries));
    }

    // Create the resulting call, which always looks like
    //
    //     let tmp1 = arg1 in
    //     let tmp2 = arg2 in
    //     ...
    //     call(tmp1, tmp2, ...)
    let mut call_expr = SymExpr::new(
        db,
        expr_span,
        input_output.output_ty,
        SymExprKind::Call {
            function,
            substitution,
            arg_temps: arg_temp_symbols.clone(),
        },
    );
    for (arg_temp_symbol, arg_expr) in arg_temp_symbols
        .into_iter()
        .rev()
        .zip(arg_exprs.into_iter().rev())
    {
        call_expr = SymExpr::new(
            db,
            call_expr.span(db),
            call_expr.ty(db),
            SymExprKind::LetIn {
                lv: arg_temp_symbol,
                ty: arg_expr.ty(db),
                initializer: Some(arg_expr),
                body: call_expr,
            },
        );
    }

    // Create the final result.
    ExprResult::from_expr(env.db(), call_expr, temporaries)
}

impl<'db> Err<'db> for ExprResult<'db> {
    fn err(db: &'db dyn dada_ir_ast::Db, r: Reported) -> Self {
        Self {
            temporaries: vec![],
            span: r.span(db),
            kind: ExprResultKind::PlaceExpr(SymPlaceExpr::err(db, r)),
        }
    }
}

impl<'db> ExprResult<'db> {
    /// Create a result based on lexical name resolution.
    pub async fn from_name_resolution(
        env: &mut Env<'db>,
        res: NameResolution<'db>,
        span: Span<'db>,
    ) -> Self {
        let db = env.db();
        match res.sym {
            NameResolutionSym::SymVariable(var) if var.kind(db) == SymGenericKind::Place => {
                let ty = env.variable_ty(var).await;
                let place_expr = SymPlaceExpr::new(db, span, ty, SymPlaceExprKind::Var(var));
                Self {
                    temporaries: vec![],
                    span,
                    kind: ExprResultKind::PlaceExpr(place_expr),
                }
            }

            // FIXME: Should functions be expressions?
            NameResolutionSym::SymFunction(_)
            | NameResolutionSym::SymModule(_)
            | NameResolutionSym::SymAggregate(_)
            | NameResolutionSym::SymPrimitive(_)
            | NameResolutionSym::SymVariable(..) => Self {
                temporaries: vec![],
                span,
                kind: ExprResultKind::Other(res),
            },
        }
    }

    pub fn from_place_expr(
        db: &'db dyn crate::Db,
        expr: SymPlaceExpr<'db>,
        temporaries: Vec<Temporary<'db>>,
    ) -> Self {
        Self {
            temporaries,
            span: expr.span(db),
            kind: ExprResultKind::PlaceExpr(expr),
        }
    }

    pub fn from_expr(
        db: &'db dyn crate::Db,
        expr: SymExpr<'db>,
        temporaries: Vec<Temporary<'db>>,
    ) -> Self {
        Self {
            temporaries,
            span: expr.span(db),
            kind: ExprResultKind::Expr(expr),
        }
    }

    /// Convert this result into an expression, with `let ... in` statements inserted for temporaries.
    pub fn into_expr_with_enclosed_temporaries(self, env: &mut Env<'db>) -> SymExpr<'db> {
        let db = env.db();
        let mut temporaries = vec![];
        let mut expr = self.into_expr(env, &mut temporaries);
        for temporary in temporaries.into_iter().rev() {
            expr = SymExpr::new(
                db,
                expr.span(db),
                expr.ty(db),
                SymExprKind::LetIn {
                    lv: temporary.lv,
                    ty: temporary.ty,
                    initializer: temporary.initializer,
                    body: expr,
                },
            );
        }

        expr
    }

    /// Computes the type of this, treating it as an expression.
    /// Reports an error if this names something that cannot be made into an expression.
    pub fn ty(&self, env: &mut Env<'db>) -> SymTy<'db> {
        let db = env.db();
        match &self.kind {
            &ExprResultKind::PlaceExpr(place_expr) => place_expr.ty(db),
            &ExprResultKind::Expr(expr) => expr.ty(db),
            ExprResultKind::Other(name_resolution) => {
                SymTy::err(db, report_non_expr(db, self.span, name_resolution))
            }
            &ExprResultKind::Method {
                self_expr: owner,
                function: method,
                ..
            } => SymTy::err(
                db,
                report_missing_call_to_method(db, owner.span(db), method),
            ),
        }
    }

    pub fn into_place_expr(
        self,
        env: &mut Env<'db>,
        temporaries: &mut Vec<Temporary<'db>>,
    ) -> SymPlaceExpr<'db> {
        let db = env.db();
        temporaries.extend(self.temporaries);
        match self.kind {
            ExprResultKind::PlaceExpr(place_expr) => place_expr,

            // This is a value that needs to be stored in a temporary.
            ExprResultKind::Expr(expr) => expr.into_temporary(db, temporaries),

            ExprResultKind::Other(name_resolution) => {
                let reported = report_non_expr(db, self.span, &name_resolution);
                SymPlaceExpr::err(db, reported)
            }

            ExprResultKind::Method {
                self_expr: owner,
                function: method,
                ..
            } => SymPlaceExpr::err(
                db,
                report_missing_call_to_method(db, owner.span(db), method),
            ),
        }
    }

    pub fn into_expr(
        self,
        env: &mut Env<'db>,
        temporaries: &mut Vec<Temporary<'db>>,
    ) -> SymExpr<'db> {
        let db = env.db();
        temporaries.extend(self.temporaries);
        match self.kind {
            ExprResultKind::Expr(expr) => expr,
            ExprResultKind::PlaceExpr(place_expr) => {
                let sym_place = place_expr.into_sym_place(db);
                SymExpr::new(
                    db,
                    place_expr.span(db),
                    place_expr.ty(db).referenced(db, sym_place),
                    SymExprKind::PermissionOp(PermissionOp::Reference, place_expr),
                )
            }

            ExprResultKind::Other(name_resolution) => {
                SymExpr::err(db, report_non_expr(db, self.span, &name_resolution))
            }
            ExprResultKind::Method {
                self_expr: owner,
                function: method,
                ..
            } => SymExpr::err(
                db,
                report_missing_call_to_method(db, owner.span(db), method),
            ),
        }
    }
}

fn report_not_implemented<'db>(db: &'db dyn crate::Db, span: Span<'db>, what: &str) -> Reported {
    Diagnostic::error(db, span, "not implemented yet :(".to_string())
        .label(
            db,
            Level::Error,
            span,
            format!("sorry, but {what} have not been implemented yet :(",),
        )
        .report(db)
}

fn report_non_expr<'db>(
    db: &'db dyn crate::Db,
    owner_span: Span<'db>,
    name_resolution: &NameResolution<'db>,
) -> Reported {
    Diagnostic::error(db, owner_span, "expected an expression".to_string())
        .label(
            db,
            Level::Error,
            owner_span,
            format!(
                "I expected to find an expression but I found {}",
                name_resolution.categorize(db),
            ),
        )
        .report(db)
}

fn report_missing_call_to_method<'db>(
    db: &'db dyn crate::Db,
    owner_span: Span<'db>,
    method: SymFunction<'db>,
) -> Reported {
    Diagnostic::error(db, owner_span, "missing call to method".to_string())
        .label(
            db,
            Level::Error,
            owner_span,
            format!(
                "`{}` is a method but you don't appear to be calling it",
                method.name(db),
            ),
        )
        .label(db, Level::Help, owner_span.at_end(), "maybe add `()` here?")
        .report(db)
}

fn report_not_callable<'db>(db: &'db dyn crate::Db, owner_span: Span<'db>) -> Reported {
    Diagnostic::error(db, owner_span, "not callable".to_string())
        .label(
            db,
            Level::Error,
            owner_span,
            "this is not something you can call".to_string(),
        )
        .report(db)
}
