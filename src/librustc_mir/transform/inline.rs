// Copyright 2016 The Rust Project Developers. See the COPYRIGHT
// file at the top-level directory of this distribution and at
// http://rust-lang.org/COPYRIGHT.
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.

//! Inlining pass for MIR functions

use rustc::hir;
use rustc::hir::def_id::DefId;

use rustc_data_structures::bitvec::BitVector;
use rustc_data_structures::indexed_vec::{Idx, IndexVec};

use rustc::mir::*;
use rustc::mir::visit::*;
use rustc::ty::{self, Instance, Ty, TyCtxt, TypeFoldable};
use rustc::ty::subst::{Subst,Substs};

use std::collections::VecDeque;
use std::iter;
use transform::{MirPass, MirSource};
use super::simplify::{remove_dead_blocks, CfgSimplifier};

use syntax::{attr};
use syntax::abi::Abi;

const DEFAULT_THRESHOLD: usize = 50;
const HINT_THRESHOLD: usize = 100;

const INSTR_COST: usize = 5;
const CALL_PENALTY: usize = 25;

const UNKNOWN_SIZE_COST: usize = 10;

pub struct Inline;

#[derive(Copy, Clone, Debug)]
struct CallSite<'tcx> {
    callee: DefId,
    substs: &'tcx Substs<'tcx>,
    bb: BasicBlock,
    location: SourceInfo,
}

impl MirPass for Inline {
    fn run_pass<'a, 'tcx>(&self,
                          tcx: TyCtxt<'a, 'tcx, 'tcx>,
                          source: MirSource,
                          mir: &mut Mir<'tcx>) {
        if tcx.sess.opts.debugging_opts.mir_opt_level >= 2 {
            Inliner { tcx, source }.run_pass(mir);
        }
    }
}

struct Inliner<'a, 'tcx: 'a> {
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    source: MirSource,
}

impl<'a, 'tcx> Inliner<'a, 'tcx> {
    fn run_pass(&self, caller_mir: &mut Mir<'tcx>) {
        // Keep a queue of callsites to try inlining on. We take
        // advantage of the fact that queries detect cycles here to
        // allow us to try and fetch the fully optimized MIR of a
        // call; if it succeeds, we can inline it and we know that
        // they do not call us.  Otherwise, we just don't try to
        // inline.
        //
        // We use a queue so that we inline "broadly" before we inline
        // in depth. It is unclear if this is the best heuristic,
        // really, but that's true of all the heuristics in this
        // file. =)

        let mut callsites = VecDeque::new();

        let param_env = self.tcx.param_env(self.source.def_id);

        // Only do inlining into fn bodies.
        let id = self.tcx.hir.as_local_node_id(self.source.def_id).unwrap();
        let body_owner_kind = self.tcx.hir.body_owner_kind(id);
        if let (hir::BodyOwnerKind::Fn, None) = (body_owner_kind, self.source.promoted) {

            for (bb, bb_data) in caller_mir.basic_blocks().iter_enumerated() {
                // Don't inline calls that are in cleanup blocks.
                if bb_data.is_cleanup { continue; }

                // Only consider direct calls to functions
                let terminator = bb_data.terminator();
                if let TerminatorKind::Call {
                    func: Operand::Constant(ref f), .. } = terminator.kind {
                        if let ty::TyFnDef(callee_def_id, substs) = f.ty.sty {
                            if let Some(instance) = Instance::resolve(self.tcx,
                                                                      param_env,
                                                                      callee_def_id,
                                                                      substs) {
                                callsites.push_back(CallSite {
                                    callee: instance.def_id(),
                                    substs: instance.substs,
                                    bb,
                                    location: terminator.source_info
                                });
                            }
                        }
                    }
            }
        } else {
            return;
        }

        let mut local_change;
        let mut changed = false;

        loop {
            local_change = false;
            while let Some(callsite) = callsites.pop_front() {
                debug!("checking whether to inline callsite {:?}", callsite);
                if !self.tcx.is_mir_available(callsite.callee) {
                    debug!("checking whether to inline callsite {:?} - MIR unavailable", callsite);
                    continue;
                }

                let callee_mir = match ty::queries::optimized_mir::try_get(self.tcx,
                                                                           callsite.location.span,
                                                                           callsite.callee) {
                    Ok(ref callee_mir) if self.should_inline(callsite, callee_mir) => {
                        subst_and_normalize(callee_mir, self.tcx, &callsite.substs, param_env)
                    }
                    Ok(_) => continue,

                    Err(mut bug) => {
                        // FIXME(#43542) shouldn't have to cancel an error
                        bug.cancel();
                        continue
                    }
                };

                let start = caller_mir.basic_blocks().len();
                debug!("attempting to inline callsite {:?} - mir={:?}", callsite, callee_mir);
                if !self.inline_call(callsite, caller_mir, callee_mir) {
                    debug!("attempting to inline callsite {:?} - failure", callsite);
                    continue;
                }
                debug!("attempting to inline callsite {:?} - success", callsite);

                // Add callsites from inlined function
                for (bb, bb_data) in caller_mir.basic_blocks().iter_enumerated().skip(start) {
                    // Only consider direct calls to functions
                    let terminator = bb_data.terminator();
                    if let TerminatorKind::Call {
                        func: Operand::Constant(ref f), .. } = terminator.kind {
                        if let ty::TyFnDef(callee_def_id, substs) = f.ty.sty {
                            // Don't inline the same function multiple times.
                            if callsite.callee != callee_def_id {
                                callsites.push_back(CallSite {
                                    callee: callee_def_id,
                                    substs,
                                    bb,
                                    location: terminator.source_info
                                });
                            }
                        }
                    }
                }

                local_change = true;
                changed = true;
            }

            if !local_change {
                break;
            }
        }

        // Simplify if we inlined anything.
        if changed {
            debug!("Running simplify cfg on {:?}", self.source);
            CfgSimplifier::new(caller_mir).simplify();
            remove_dead_blocks(caller_mir);
        }
    }

    fn should_inline(&self,
                     callsite: CallSite<'tcx>,
                     callee_mir: &Mir<'tcx>)
                     -> bool
    {
        debug!("should_inline({:?})", callsite);
        let tcx = self.tcx;

        // Don't inline closures that have captures
        // FIXME: Handle closures better
        if callee_mir.upvar_decls.len() > 0 {
            debug!("    upvar decls present - not inlining");
            return false;
        }

        // Cannot inline generators which haven't been transformed yet
        if callee_mir.yield_ty.is_some() {
            debug!("    yield ty present - not inlining");
            return false;
        }

        let attrs = tcx.get_attrs(callsite.callee);
        let hint = attr::find_inline_attr(None, &attrs[..]);

        let hinted = match hint {
            // Just treat inline(always) as a hint for now,
            // there are cases that prevent inlining that we
            // need to check for first.
            attr::InlineAttr::Always => true,
            attr::InlineAttr::Never => {
                debug!("#[inline(never)] present - not inlining");
                return false
            }
            attr::InlineAttr::Hint => true,
            attr::InlineAttr::None => false,
        };

        // Only inline local functions if they would be eligible for cross-crate
        // inlining. This is to ensure that the final crate doesn't have MIR that
        // reference unexported symbols
        if callsite.callee.is_local() {
            if callsite.substs.types().count() == 0 && !hinted {
                debug!("    callee is an exported function - not inlining");
                return false;
            }
        }

        let mut threshold = if hinted {
            HINT_THRESHOLD
        } else {
            DEFAULT_THRESHOLD
        };

        // Significantly lower the threshold for inlining cold functions
        if attr::contains_name(&attrs[..], "cold") {
            threshold /= 5;
        }

        // Give a bonus functions with a small number of blocks,
        // We normally have two or three blocks for even
        // very small functions.
        if callee_mir.basic_blocks().len() <= 3 {
            threshold += threshold / 4;
        }
        debug!("    final inline threshold = {}", threshold);

        // FIXME: Give a bonus to functions with only a single caller

        let param_env = tcx.param_env(self.source.def_id);

        let mut first_block = true;
        let mut cost = 0;

        // Traverse the MIR manually so we can account for the effects of
        // inlining on the CFG.
        let mut work_list = vec![START_BLOCK];
        let mut visited = BitVector::new(callee_mir.basic_blocks().len());
        while let Some(bb) = work_list.pop() {
            if !visited.insert(bb.index()) { continue; }
            let blk = &callee_mir.basic_blocks()[bb];

            for stmt in &blk.statements {
                // Don't count StorageLive/StorageDead in the inlining cost.
                match stmt.kind {
                    StatementKind::StorageLive(_) |
                    StatementKind::StorageDead(_) |
                    StatementKind::Nop => {}
                    _ => cost += INSTR_COST
                }
            }
            let term = blk.terminator();
            let mut is_drop = false;
            match term.kind {
                TerminatorKind::Drop { ref location, target, unwind } |
                TerminatorKind::DropAndReplace { ref location, target, unwind, .. } => {
                    is_drop = true;
                    work_list.push(target);
                    // If the location doesn't actually need dropping, treat it like
                    // a regular goto.
                    let ty = location.ty(callee_mir, tcx).subst(tcx, callsite.substs);
                    let ty = ty.to_ty(tcx);
                    if ty.needs_drop(tcx, param_env) {
                        cost += CALL_PENALTY;
                        if let Some(unwind) = unwind {
                            work_list.push(unwind);
                        }
                    } else {
                        cost += INSTR_COST;
                    }
                }

                TerminatorKind::Unreachable |
                TerminatorKind::Call { destination: None, .. } if first_block => {
                    // If the function always diverges, don't inline
                    // unless the cost is zero
                    threshold = 0;
                }

                TerminatorKind::Call {func: Operand::Constant(ref f), .. } => {
                    if let ty::TyFnDef(def_id, _) = f.ty.sty {
                        // Don't give intrinsics the extra penalty for calls
                        let f = tcx.fn_sig(def_id);
                        if f.abi() == Abi::RustIntrinsic || f.abi() == Abi::PlatformIntrinsic {
                            cost += INSTR_COST;
                        } else {
                            cost += CALL_PENALTY;
                        }
                    }
                }
                TerminatorKind::Assert { .. } => cost += CALL_PENALTY,
                _ => cost += INSTR_COST
            }

            if !is_drop {
                for &succ in &term.successors()[..] {
                    work_list.push(succ);
                }
            }

            first_block = false;
        }

        // Count up the cost of local variables and temps, if we know the size
        // use that, otherwise we use a moderately-large dummy cost.

        let ptr_size = tcx.data_layout.pointer_size.bytes();

        for v in callee_mir.vars_and_temps_iter() {
            let v = &callee_mir.local_decls[v];
            let ty = v.ty.subst(tcx, callsite.substs);
            // Cost of the var is the size in machine-words, if we know
            // it.
            if let Some(size) = type_size_of(tcx, param_env.clone(), ty) {
                cost += (size / ptr_size) as usize;
            } else {
                cost += UNKNOWN_SIZE_COST;
            }
        }

        if let attr::InlineAttr::Always = hint {
            debug!("INLINING {:?} because inline(always) [cost={}]", callsite, cost);
            true
        } else {
            if cost <= threshold {
                debug!("INLINING {:?} [cost={} <= threshold={}]", callsite, cost, threshold);
                true
            } else {
                debug!("NOT inlining {:?} [cost={} > threshold={}]", callsite, cost, threshold);
                false
            }
        }
    }

    fn inline_call(&self,
                   callsite: CallSite<'tcx>,
                   caller_mir: &mut Mir<'tcx>,
                   mut callee_mir: Mir<'tcx>) -> bool {
        let terminator = caller_mir[callsite.bb].terminator.take().unwrap();
        match terminator.kind {
            // FIXME: Handle inlining of diverging calls
            TerminatorKind::Call { args, destination: Some(destination), cleanup, .. } => {
                debug!("Inlined {:?} into {:?}", callsite.callee, self.source);

                let is_box_free = Some(callsite.callee) == self.tcx.lang_items().box_free_fn();

                let mut local_map = IndexVec::with_capacity(callee_mir.local_decls.len());
                let mut scope_map = IndexVec::with_capacity(callee_mir.visibility_scopes.len());
                let mut promoted_map = IndexVec::with_capacity(callee_mir.promoted.len());

                for mut scope in callee_mir.visibility_scopes.iter().cloned() {
                    if scope.parent_scope.is_none() {
                        scope.parent_scope = Some(callsite.location.scope);
                        scope.span = callee_mir.span;
                    }

                    scope.span = callsite.location.span;

                    let idx = caller_mir.visibility_scopes.push(scope);
                    scope_map.push(idx);
                }

                for loc in callee_mir.vars_and_temps_iter() {
                    let mut local = callee_mir.local_decls[loc].clone();

                    local.source_info.scope = scope_map[local.source_info.scope];
                    local.source_info.span = callsite.location.span;

                    let idx = caller_mir.local_decls.push(local);
                    local_map.push(idx);
                }

                for p in callee_mir.promoted.iter().cloned() {
                    let idx = caller_mir.promoted.push(p);
                    promoted_map.push(idx);
                }

                // If the call is something like `a[*i] = f(i)`, where
                // `i : &mut usize`, then just duplicating the `a[*i]`
                // Place could result in two different locations if `f`
                // writes to `i`. To prevent this we need to create a temporary
                // borrow of the place and pass the destination as `*temp` instead.
                fn dest_needs_borrow(place: &Place) -> bool {
                    match *place {
                        Place::Projection(ref p) => {
                            match p.elem {
                                ProjectionElem::Deref |
                                ProjectionElem::Index(_) => true,
                                _ => dest_needs_borrow(&p.base)
                            }
                        }
                        // Static variables need a borrow because the callee
                        // might modify the same static.
                        Place::Static(_) => true,
                        _ => false
                    }
                }

                let dest = if dest_needs_borrow(&destination.0) {
                    debug!("Creating temp for return destination");
                    let dest = Rvalue::Ref(
                        self.tcx.types.re_erased,
                        BorrowKind::Mut,
                        destination.0);

                    let ty = dest.ty(caller_mir, self.tcx);

                    let temp = LocalDecl::new_temp(ty, callsite.location.span);

                    let tmp = caller_mir.local_decls.push(temp);
                    let tmp = Place::Local(tmp);

                    let stmt = Statement {
                        source_info: callsite.location,
                        kind: StatementKind::Assign(tmp.clone(), dest)
                    };
                    caller_mir[callsite.bb]
                        .statements.push(stmt);
                    tmp.deref()
                } else {
                    destination.0
                };

                let return_block = destination.1;

                let args : Vec<_> = if is_box_free {
                    assert!(args.len() == 1);
                    // box_free takes a Box, but is defined with a *mut T, inlining
                    // needs to generate the cast.
                    // FIXME: we should probably just generate correct MIR in the first place...

                    let arg = if let Operand::Move(ref place) = args[0] {
                        place.clone()
                    } else {
                        bug!("Constant arg to \"box_free\"");
                    };

                    let ptr_ty = args[0].ty(caller_mir, self.tcx);
                    vec![self.cast_box_free_arg(arg, ptr_ty, &callsite, caller_mir)]
                } else {
                    // Copy the arguments if needed.
                    self.make_call_args(args, &callsite, caller_mir)
                };

                let bb_len = caller_mir.basic_blocks().len();
                let mut integrator = Integrator {
                    block_idx: bb_len,
                    args: &args,
                    local_map,
                    scope_map,
                    promoted_map,
                    _callsite: callsite,
                    destination: dest,
                    return_block,
                    cleanup_block: cleanup,
                    in_cleanup_block: false
                };


                for (bb, mut block) in callee_mir.basic_blocks_mut().drain_enumerated(..) {
                    integrator.visit_basic_block_data(bb, &mut block);
                    caller_mir.basic_blocks_mut().push(block);
                }

                let terminator = Terminator {
                    source_info: callsite.location,
                    kind: TerminatorKind::Goto { target: BasicBlock::new(bb_len) }
                };

                caller_mir[callsite.bb].terminator = Some(terminator);

                true
            }
            kind => {
                caller_mir[callsite.bb].terminator = Some(Terminator {
                    source_info: terminator.source_info,
                    kind,
                });
                false
            }
        }
    }

    fn cast_box_free_arg(&self, arg: Place<'tcx>, ptr_ty: Ty<'tcx>,
                         callsite: &CallSite<'tcx>, caller_mir: &mut Mir<'tcx>) -> Local {
        let arg = Rvalue::Ref(
            self.tcx.types.re_erased,
            BorrowKind::Mut,
            arg.deref());

        let ty = arg.ty(caller_mir, self.tcx);
        let ref_tmp = LocalDecl::new_temp(ty, callsite.location.span);
        let ref_tmp = caller_mir.local_decls.push(ref_tmp);
        let ref_tmp = Place::Local(ref_tmp);

        let ref_stmt = Statement {
            source_info: callsite.location,
            kind: StatementKind::Assign(ref_tmp.clone(), arg)
        };

        caller_mir[callsite.bb]
            .statements.push(ref_stmt);

        let pointee_ty = match ptr_ty.sty {
            ty::TyRawPtr(tm) | ty::TyRef(_, tm) => tm.ty,
            _ if ptr_ty.is_box() => ptr_ty.boxed_ty(),
            _ => bug!("Invalid type `{:?}` for call to box_free", ptr_ty)
        };
        let ptr_ty = self.tcx.mk_mut_ptr(pointee_ty);

        let raw_ptr = Rvalue::Cast(CastKind::Misc, Operand::Move(ref_tmp), ptr_ty);

        let cast_tmp = LocalDecl::new_temp(ptr_ty, callsite.location.span);
        let cast_tmp = caller_mir.local_decls.push(cast_tmp);

        let cast_stmt = Statement {
            source_info: callsite.location,
            kind: StatementKind::Assign(Place::Local(cast_tmp), raw_ptr)
        };

        caller_mir[callsite.bb]
            .statements.push(cast_stmt);

        cast_tmp
    }

    fn make_call_args(
        &self,
        args: Vec<Operand<'tcx>>,
        callsite: &CallSite<'tcx>,
        caller_mir: &mut Mir<'tcx>,
    ) -> Vec<Local> {
        let tcx = self.tcx;

        // There is a bit of a mismatch between the *caller* of a closure and the *callee*.
        // The caller provides the arguments wrapped up in a tuple:
        //
        //     tuple_tmp = (a, b, c)
        //     Fn::call(closure_ref, tuple_tmp)
        //
        // meanwhile the closure body expects the arguments (here, `a`, `b`, and `c`)
        // as distinct arguments. (This is the "rust-call" ABI hack.) Normally, trans has
        // the job of unpacking this tuple. But here, we are trans. =) So we want to create
        // a vector like
        //
        //     [closure_ref, tuple_tmp.0, tuple_tmp.1, tuple_tmp.2]
        //
        // Except for one tiny wrinkle: we don't actually want `tuple_tmp.0`. It's more convenient
        // if we "spill" that into *another* temporary, so that we can map the argument
        // variable in the callee MIR directly to an argument variable on our side.
        // So we introduce temporaries like:
        //
        //     tmp0 = tuple_tmp.0
        //     tmp1 = tuple_tmp.1
        //     tmp2 = tuple_tmp.2
        //
        // and the vector is `[closure_ref, tmp0, tmp1, tmp2]`.
        if tcx.is_closure(callsite.callee) {
            let mut args = args.into_iter();
            let self_ = self.create_temp_if_necessary(args.next().unwrap(), callsite, caller_mir);
            let tuple = self.create_temp_if_necessary(args.next().unwrap(), callsite, caller_mir);
            assert!(args.next().is_none());

            let tuple = Place::Local(tuple);
            let tuple_tys = if let ty::TyTuple(s, _) = tuple.ty(caller_mir, tcx).to_ty(tcx).sty {
                s
            } else {
                bug!("Closure arguments are not passed as a tuple");
            };

            // The `closure_ref` in our example above.
            let closure_ref_arg = iter::once(self_);

            // The `tmp0`, `tmp1`, and `tmp2` in our example abonve.
            let tuple_tmp_args =
                tuple_tys.iter().enumerate().map(|(i, ty)| {
                    // This is e.g. `tuple_tmp.0` in our example above.
                    let tuple_field = Operand::Move(tuple.clone().field(Field::new(i), ty));

                    // Spill to a local to make e.g. `tmp0`.
                    self.create_temp_if_necessary(tuple_field, callsite, caller_mir)
                });

            closure_ref_arg.chain(tuple_tmp_args).collect()
        } else {
            args.into_iter()
                .map(|a| self.create_temp_if_necessary(a, callsite, caller_mir))
                .collect()
        }
    }

    /// If `arg` is already a temporary, returns it. Otherwise, introduces a fresh
    /// temporary `T` and an instruction `T = arg`, and returns `T`.
    fn create_temp_if_necessary(
        &self,
        arg: Operand<'tcx>,
        callsite: &CallSite<'tcx>,
        caller_mir: &mut Mir<'tcx>,
    ) -> Local {
        // FIXME: Analysis of the usage of the arguments to avoid
        // unnecessary temporaries.

        if let Operand::Move(Place::Local(local)) = arg {
            if caller_mir.local_kind(local) == LocalKind::Temp {
                // Reuse the operand if it's a temporary already
                return local;
            }
        }

        debug!("Creating temp for argument {:?}", arg);
        // Otherwise, create a temporary for the arg
        let arg = Rvalue::Use(arg);

        let ty = arg.ty(caller_mir, self.tcx);

        let arg_tmp = LocalDecl::new_temp(ty, callsite.location.span);
        let arg_tmp = caller_mir.local_decls.push(arg_tmp);

        let stmt = Statement {
            source_info: callsite.location,
            kind: StatementKind::Assign(Place::Local(arg_tmp), arg),
        };
        caller_mir[callsite.bb].statements.push(stmt);
        arg_tmp
    }
}

fn type_size_of<'a, 'tcx>(tcx: TyCtxt<'a, 'tcx, 'tcx>,
                          param_env: ty::ParamEnv<'tcx>,
                          ty: Ty<'tcx>) -> Option<u64> {
    tcx.layout_of(param_env.and(ty)).ok().map(|layout| layout.size.bytes())
}

fn subst_and_normalize<'a, 'tcx: 'a>(
    mir: &Mir<'tcx>,
    tcx: TyCtxt<'a, 'tcx, 'tcx>,
    substs: &'tcx ty::subst::Substs<'tcx>,
    param_env: ty::ParamEnv<'tcx>,
) -> Mir<'tcx> {
    struct Folder<'a, 'tcx: 'a> {
        tcx: TyCtxt<'a, 'tcx, 'tcx>,
        param_env: ty::ParamEnv<'tcx>,
        substs: &'tcx ty::subst::Substs<'tcx>,
    }
    impl<'a, 'tcx: 'a> ty::fold::TypeFolder<'tcx, 'tcx> for Folder<'a, 'tcx> {
        fn tcx<'b>(&'b self) -> TyCtxt<'b, 'tcx, 'tcx> {
            self.tcx
        }

        fn fold_ty(&mut self, t: Ty<'tcx>) -> Ty<'tcx> {
            self.tcx.trans_apply_param_substs_env(&self.substs, self.param_env, &t)
        }
    }
    let mut f = Folder { tcx, param_env, substs };
    mir.fold_with(&mut f)
}

/**
 * Integrator.
 *
 * Integrates blocks from the callee function into the calling function.
 * Updates block indices, references to locals and other control flow
 * stuff.
 */
struct Integrator<'a, 'tcx: 'a> {
    block_idx: usize,
    args: &'a [Local],
    local_map: IndexVec<Local, Local>,
    scope_map: IndexVec<VisibilityScope, VisibilityScope>,
    promoted_map: IndexVec<Promoted, Promoted>,
    _callsite: CallSite<'tcx>,
    destination: Place<'tcx>,
    return_block: BasicBlock,
    cleanup_block: Option<BasicBlock>,
    in_cleanup_block: bool,
}

impl<'a, 'tcx> Integrator<'a, 'tcx> {
    fn update_target(&self, tgt: BasicBlock) -> BasicBlock {
        let new = BasicBlock::new(tgt.index() + self.block_idx);
        debug!("Updating target `{:?}`, new: `{:?}`", tgt, new);
        new
    }
}

impl<'a, 'tcx> MutVisitor<'tcx> for Integrator<'a, 'tcx> {
    fn visit_local(&mut self,
                   local: &mut Local,
                   _ctxt: PlaceContext<'tcx>,
                   _location: Location) {
        if *local == RETURN_PLACE {
            match self.destination {
                Place::Local(l) => {
                    *local = l;
                    return;
                },
                ref place => bug!("Return place is {:?}, not local", place)
            }
        }
        let idx = local.index() - 1;
        if idx < self.args.len() {
            *local = self.args[idx];
            return;
        }
        *local = self.local_map[Local::new(idx - self.args.len())];
    }

    fn visit_place(&mut self,
                    place: &mut Place<'tcx>,
                    _ctxt: PlaceContext<'tcx>,
                    _location: Location) {
        if let Place::Local(RETURN_PLACE) = *place {
            // Return pointer; update the place itself
            *place = self.destination.clone();
        } else {
            self.super_place(place, _ctxt, _location);
        }
    }

    fn visit_basic_block_data(&mut self, block: BasicBlock, data: &mut BasicBlockData<'tcx>) {
        self.in_cleanup_block = data.is_cleanup;
        self.super_basic_block_data(block, data);
        self.in_cleanup_block = false;
    }

    fn visit_terminator_kind(&mut self, block: BasicBlock,
                             kind: &mut TerminatorKind<'tcx>, loc: Location) {
        self.super_terminator_kind(block, kind, loc);

        match *kind {
            TerminatorKind::GeneratorDrop |
            TerminatorKind::Yield { .. } => bug!(),
            TerminatorKind::Goto { ref mut target} => {
                *target = self.update_target(*target);
            }
            TerminatorKind::SwitchInt { ref mut targets, .. } => {
                for tgt in targets {
                    *tgt = self.update_target(*tgt);
                }
            }
            TerminatorKind::Drop { ref mut target, ref mut unwind, .. } |
            TerminatorKind::DropAndReplace { ref mut target, ref mut unwind, .. } => {
                *target = self.update_target(*target);
                if let Some(tgt) = *unwind {
                    *unwind = Some(self.update_target(tgt));
                } else if !self.in_cleanup_block {
                    // Unless this drop is in a cleanup block, add an unwind edge to
                    // the orignal call's cleanup block
                    *unwind = self.cleanup_block;
                }
            }
            TerminatorKind::Call { ref mut destination, ref mut cleanup, .. } => {
                if let Some((_, ref mut tgt)) = *destination {
                    *tgt = self.update_target(*tgt);
                }
                if let Some(tgt) = *cleanup {
                    *cleanup = Some(self.update_target(tgt));
                } else if !self.in_cleanup_block {
                    // Unless this call is in a cleanup block, add an unwind edge to
                    // the orignal call's cleanup block
                    *cleanup = self.cleanup_block;
                }
            }
            TerminatorKind::Assert { ref mut target, ref mut cleanup, .. } => {
                *target = self.update_target(*target);
                if let Some(tgt) = *cleanup {
                    *cleanup = Some(self.update_target(tgt));
                } else if !self.in_cleanup_block {
                    // Unless this assert is in a cleanup block, add an unwind edge to
                    // the orignal call's cleanup block
                    *cleanup = self.cleanup_block;
                }
            }
            TerminatorKind::Return => {
                *kind = TerminatorKind::Goto { target: self.return_block };
            }
            TerminatorKind::Resume => {
                if let Some(tgt) = self.cleanup_block {
                    *kind = TerminatorKind::Goto { target: tgt }
                }
            }
            TerminatorKind::Abort => { }
            TerminatorKind::Unreachable => { }
            TerminatorKind::FalseEdges { ref mut real_target, ref mut imaginary_targets } => {
                *real_target = self.update_target(*real_target);
                for target in imaginary_targets {
                    *target = self.update_target(*target);
                }
            }
        }
    }

    fn visit_visibility_scope(&mut self, scope: &mut VisibilityScope) {
        *scope = self.scope_map[*scope];
    }

    fn visit_literal(&mut self, literal: &mut Literal<'tcx>, loc: Location) {
        if let Literal::Promoted { ref mut index } = *literal {
            if let Some(p) = self.promoted_map.get(*index).cloned() {
                *index = p;
            }
        } else {
            self.super_literal(literal, loc);
        }
    }
}
