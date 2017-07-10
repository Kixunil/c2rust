use std::collections::{HashMap, HashSet};
use rustc::hir;
use rustc::hir::def::Def;
use rustc::hir::def_id::DefId;
use rustc::ty::TypeVariants;
use syntax::abi::Abi;
use syntax::ast::*;
use syntax::codemap::{DUMMY_SP, Spanned};
use syntax::fold::{self, Folder};
use syntax::ptr::P;
use syntax::symbol::Symbol;
use syntax::util::ThinVec;
use syntax::util::small_vector::SmallVector;

use api::*;
use ast_equiv::AstEquiv;
use bindings::Bindings;
use bindings::IntoSymbol;
use dataflow;
use driver::{self, Phase};
use fold::Fold;
use fn_edit::FnLike;
use transform::Transform;
use util::Lone;


/// Turn free functions into methods in an impl.  
pub struct ToMethod;

impl Transform for ToMethod {
    fn transform(&self, krate: Crate, cx: &driver::Ctxt) -> Crate {
        // (1) Find the impl we're inserting into.

        let mut dest = None;

        let krate = fold_nodes(krate, |i: P<Item>| {
            // We're looking for an inherent impl (no `TraitRef`) marked with a cursor.
            if !cx.has_cursor(&i) ||
               !matches!([i.node] ItemKind::Impl(_, _, _, None, _, _)) {
                return SmallVector::one(i);
            }

            if dest.is_none() {
                dest = Some(i.clone());
            }

            SmallVector::one(i)
        });

        if dest.is_none() {
            return krate;
        }
        let dest = dest.unwrap();


        // (2) Collect all marked functions, removing them from the AST.  Note that we collect only
        // free functions, not trait or impl methods.

        struct FnInfo {
            item: P<Item>,

            decl: P<FnDecl>,
            unsafety: Unsafety,
            constness: Spanned<Constness>,
            abi: Abi,
            generics: Generics,
            block: P<Block>,

            arg_idx: usize,
        }
        let mut fns = Vec::new();

        let krate = fold_modules(krate, |curs| {
            while let Some(arg_idx) = curs.advance_until_match(|i| {
                // Find the argument under the cursor.
                if !cx.has_cursor(&i) {
                    return None;
                }
                let decl = match_or!([i.node] ItemKind::Fn(ref decl, ..) => decl; return None);
                for (idx, arg) in decl.inputs.iter().enumerate() {
                    if cx.has_cursor(&arg.pat) || cx.has_cursor(&arg.ty) ||
                       (arg.pat.span.ctxt == arg.ty.span.ctxt &&
                        cx.span_has_cursor(arg.pat.span.between(arg.ty.span))) {
                        return Some(idx);
                    }
                }
                None
            }) {
                let i = curs.remove();
                unpack!([i.node.clone()]
                        ItemKind::Fn(decl, unsafety, constness, abi, generics, block));
                fns.push(FnInfo {
                    item: i,
                    decl, unsafety, constness, abi, generics, block,
                    arg_idx,
                });
            }
        });

        // Build a hash table with info needed to rewrite references to marked functions.
        struct FnRefInfo {
            ident: Ident,
            arg_idx: usize,
        }
        let mut fn_ref_info = fns.iter().map(|f| {
            (cx.node_def_id(f.item.id),
             FnRefInfo {
                 ident: f.item.ident.clone(),
                 arg_idx: f.arg_idx,
             })
        }).collect::<HashMap<_, _>>();


        // (3) Rewrite function signatures and bodies, replacing the marked arg with `self`.
        for f in &mut fns {
            let mut inputs = f.decl.inputs.clone();

            // Remove the marked arg and inspect it.
            let arg = inputs.remove(f.arg_idx);

            let (mode, ident) = match arg.pat.node {
                PatKind::Ident(mode, ident, _) => (mode, ident),
                _ => panic!("unsupported argument pattern (expected ident): {:?}", arg.pat),
            };

            let pat_ty = cx.node_type(arg.pat.id);
            let self_ty = cx.def_type(cx.node_def_id(dest.id));
            let arg_def_id = cx.node_def_id(arg.pat.id);

            // Build the new `self` argument and insert it.
            let self_kind = {
                if pat_ty == self_ty {
                    match mode {
                        BindingMode::ByValue(mutbl) => Some(SelfKind::Value(mutbl)),
                        BindingMode::ByRef(mutbl) => Some(SelfKind::Region(None, mutbl)),
                    }
                } else {
                    match pat_ty.sty {
                        TypeVariants::TyRef(rgn, tym) if tym.ty == self_ty => {
                            match arg.ty.node {
                                TyKind::Rptr(ref lt, ref mty) =>
                                    Some(SelfKind::Region(lt.clone(), mty.mutbl)),
                                _ => None,
                            }
                        },
                        _ => None,
                    }
                }
            };
            let self_kind = match self_kind {
                Some(x) => x,
                None => panic!("unsupported argument type (expected {:?} or a ref): {:?}",
                               self_ty, pat_ty),
            };

            inputs.insert(0, mk().self_arg(self_kind));

            // Update `decl`
            f.decl = f.decl.clone().map(|fd| FnDecl { inputs: inputs, .. fd });

            // Rewrite references to the marked argument within the function body.
            f.block = fold_resolved_paths(f.block.clone(), cx, |qself, path, def_id| {
                if def_id == arg_def_id {
                    assert!(qself.is_none());
                    (None, mk().path(vec!["self"]))
                } else {
                    (qself, path)
                }
            });
        }


        // (4) Find the destination impl again, and fill it in with the new methods.

        let mut fns = Some(fns);

        let krate = fold_nodes(krate, |i: P<Item>| {
            if i.id != dest.id || fns.is_none() {
                return SmallVector::one(i);
            }

            SmallVector::one(i.map(|i| {
                unpack!([i.node] ItemKind::Impl(
                        unsafety, polarity, generics, trait_ref, ty, items));
                let mut items = items;
                let fns = fns.take().unwrap();
                items.extend(fns.into_iter().map(|f| {
                    let sig = MethodSig {
                        unsafety: f.unsafety,
                        constness: f.constness,
                        abi: f.abi,
                        decl: f.decl,
                        generics: f.generics,
                    };
                    ImplItem {
                        id: DUMMY_NODE_ID,
                        ident: f.item.ident.clone(),
                        vis: f.item.vis.clone(),
                        defaultness: Defaultness::Final,
                        attrs: f.item.attrs.clone(),
                        node: ImplItemKind::Method(sig, f.block),
                        span: f.item.span,
                    }
                }));
                Item {
                    node: ItemKind::Impl(
                              unsafety, polarity, generics, trait_ref, ty, items),
                    .. i
                }
            }))
        });


        // (5) Find all uses of marked functions, and rewrite them into method calls.

        let krate = fold_nodes(krate, |e: P<Expr>| {
            if !matches!([e.node] ExprKind::Call(..)) {
                return e;
            }

            unpack!([e.node.clone()] ExprKind::Call(func, args));
            let def_id = match_or!([cx.try_resolve_expr(&func)] Some(x) => x; return e);
            let info = match_or!([fn_ref_info.get(&def_id)] Some(x) => x; return e);

            // At this point, we know `func` is a reference to a marked function, and we have the
            // function's `FnRefInfo`.

            let mut args = args;
            let self_arg = args.remove(info.arg_idx);
            args.insert(0, self_arg);

            e.map(|e| {
                Expr {
                    node: ExprKind::MethodCall(
                              mk().spanned(info.ident.clone()),
                              vec![],
                              args),
                    .. e
                }
            })
        });


        krate
    }

    fn min_phase(&self) -> Phase {
        Phase::Phase3
    }
}


/// Find unused `unsafe` blocks and turn them into ordinary blocks.
pub struct FixUnusedUnsafe;

impl Transform for FixUnusedUnsafe {
    fn transform(&self, krate: Crate, cx: &driver::Ctxt) -> Crate {
        let krate = fold_nodes(krate, |b: P<Block>| {
            if b.rules == BlockCheckMode::Unsafe(UnsafeSource::UserProvided) &&
               !cx.ty_ctxt().used_unsafe.borrow().contains(&b.id) {
                b.map(|b| Block {
                    rules: BlockCheckMode::Default,
                    .. b
                })
            } else {
                b
            }
        });

        krate
    }

    fn min_phase(&self) -> Phase {
        Phase::Phase3
    }
}


/// Turn `unsafe fn f() { ... }` into `fn f() { unsafe { ... } }`.
pub struct SinkUnsafe;

struct SinkUnsafeFolder<'a, 'hir: 'a, 'gcx: 'a + 'tcx, 'tcx: 'a> {
    cx: &'a driver::Ctxt<'a, 'hir, 'gcx, 'tcx>,
}

impl<'a, 'hir, 'gcx, 'tcx> Folder for SinkUnsafeFolder<'a, 'hir, 'gcx, 'tcx> {
    fn fold_item(&mut self, i: P<Item>) -> SmallVector<P<Item>> {
        let i =
            if self.cx.has_cursor(&i) &&
               matches!([i.node] ItemKind::Fn(_, Unsafety::Unsafe, _, _, _, _)) {
                i.map(|mut i| {
                    match i.node {
                        ItemKind::Fn(_, ref mut unsafety, _, _, _, ref mut block) => {
                            *unsafety = Unsafety::Normal;
                            *block = mk().block(vec![
                                mk().expr_stmt(mk().block_expr(mk().unsafe_().block(
                                            block.stmts.clone())))]);
                        },
                        _ => unreachable!(),
                    }
                    i
                })
            } else {
                i
            };

        fold::noop_fold_item(i, self)
    }
}

impl Transform for SinkUnsafe {
    fn transform(&self, krate: Crate, cx: &driver::Ctxt) -> Crate {
        krate.fold(&mut SinkUnsafeFolder { cx })
    }
}
