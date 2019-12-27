use crate::utils::{
    has_drop, is_copy, match_def_path, match_type, paths, snippet_opt, span_lint_hir, span_lint_hir_and_then,
    walk_ptrs_ty_depth,
};
use if_chain::if_chain;
use matches::matches;
use rustc::declare_lint_pass;
use rustc::hir::intravisit::FnKind;
use rustc::hir::{def_id, Body, FnDecl, HirId};
use rustc::lint::{LateContext, LateLintPass, LintArray, LintPass};
use rustc::mir::{
    self, traversal,
    visit::{MutatingUseContext, PlaceContext, Visitor as _},
};
use rustc::ty::{self, fold::TypeVisitor, Ty};
use rustc_data_structures::{fx::FxHashMap, transitive_relation::TransitiveRelation};
use rustc_errors::Applicability;
use rustc_index::bit_set::{BitSet, HybridBitSet};
use rustc_mir::dataflow::{
    do_dataflow, BitDenotation, BottomValue, DataflowResults, DataflowResultsCursor, DebugFormatted, GenKillSet,
};
use rustc_session::declare_tool_lint;
use std::convert::TryFrom;
use syntax::source_map::{BytePos, Span};

macro_rules! unwrap_or_continue {
    ($x:expr) => {
        match $x {
            Some(x) => x,
            None => continue,
        }
    };
}

declare_clippy_lint! {
    /// **What it does:** Checks for a redundant `clone()` (and its relatives) which clones an owned
    /// value that is going to be dropped without further use.
    ///
    /// **Why is this bad?** It is not always possible for the compiler to eliminate useless
    /// allocations and deallocations generated by redundant `clone()`s.
    ///
    /// **Known problems:**
    ///
    /// False-negatives: analysis performed by this lint is conservative and limited.
    ///
    /// **Example:**
    /// ```rust
    /// # use std::path::Path;
    /// # #[derive(Clone)]
    /// # struct Foo;
    /// # impl Foo {
    /// #     fn new() -> Self { Foo {} }
    /// # }
    /// # fn call(x: Foo) {}
    /// {
    ///     let x = Foo::new();
    ///     call(x.clone());
    ///     call(x.clone()); // this can just pass `x`
    /// }
    ///
    /// ["lorem", "ipsum"].join(" ").to_string();
    ///
    /// Path::new("/a/b").join("c").to_path_buf();
    /// ```
    pub REDUNDANT_CLONE,
    perf,
    "`clone()` of an owned value that is going to be dropped immediately"
}

declare_lint_pass!(RedundantClone => [REDUNDANT_CLONE]);

impl<'a, 'tcx> LateLintPass<'a, 'tcx> for RedundantClone {
    #[allow(clippy::too_many_lines)]
    fn check_fn(
        &mut self,
        cx: &LateContext<'a, 'tcx>,
        _: FnKind<'tcx>,
        _: &'tcx FnDecl,
        body: &'tcx Body<'_>,
        _: Span,
        _: HirId,
    ) {
        let def_id = cx.tcx.hir().body_owner_def_id(body.id());
        let mir = cx.tcx.optimized_mir(def_id);
        let mir_read_only = mir.unwrap_read_only();

        let dead_unwinds = BitSet::new_empty(mir.basic_blocks().len());
        let maybe_storage_live_result = do_dataflow(
            cx.tcx,
            mir,
            def_id,
            &[],
            &dead_unwinds,
            MaybeStorageLive::new(mir),
            |bd, p| DebugFormatted::new(&bd.body.local_decls[p]),
        );
        let mut possible_borrower = {
            let mut vis = PossibleBorrowerVisitor::new(cx, mir);
            vis.visit_body(mir_read_only);
            vis.into_map(cx, maybe_storage_live_result)
        };

        for (bb, bbdata) in mir.basic_blocks().iter_enumerated() {
            let terminator = bbdata.terminator();

            if terminator.source_info.span.from_expansion() {
                continue;
            }

            // Give up on loops
            if terminator.successors().any(|s| *s == bb) {
                continue;
            }

            let (fn_def_id, arg, arg_ty, _) = unwrap_or_continue!(is_call_with_ref_arg(cx, mir, &terminator.kind));

            let from_borrow = match_def_path(cx, fn_def_id, &paths::CLONE_TRAIT_METHOD)
                || match_def_path(cx, fn_def_id, &paths::TO_OWNED_METHOD)
                || (match_def_path(cx, fn_def_id, &paths::TO_STRING_METHOD) && match_type(cx, arg_ty, &paths::STRING));

            let from_deref = !from_borrow
                && (match_def_path(cx, fn_def_id, &paths::PATH_TO_PATH_BUF)
                    || match_def_path(cx, fn_def_id, &paths::OS_STR_TO_OS_STRING));

            if !from_borrow && !from_deref {
                continue;
            }

            // `{ cloned = &arg; clone(move cloned); }` or `{ cloned = &arg; to_path_buf(cloned); }`
            let (cloned, cannot_move_out) = unwrap_or_continue!(find_stmt_assigns_to(cx, mir, arg, from_borrow, bb));

            let loc = mir::Location {
                block: bb,
                statement_index: bbdata.statements.len(),
            };

            // Cloned local
            let local = if from_borrow {
                // `res = clone(arg)` can be turned into `res = move arg;`
                // if `arg` is the only borrow of `cloned` at this point.

                if cannot_move_out || !possible_borrower.only_borrowers(&[arg], cloned, loc) {
                    continue;
                }

                cloned
            } else {
                // `arg` is a reference as it is `.deref()`ed in the previous block.
                // Look into the predecessor block and find out the source of deref.

                let ps = mir_read_only.predecessors_for(bb);
                if ps.len() != 1 {
                    continue;
                }
                let pred_terminator = mir[ps[0]].terminator();

                // receiver of the `deref()` call
                let pred_arg = if_chain! {
                    if let Some((pred_fn_def_id, pred_arg, pred_arg_ty, Some(res))) =
                        is_call_with_ref_arg(cx, mir, &pred_terminator.kind);
                    if res.base == mir::PlaceBase::Local(cloned);
                    if match_def_path(cx, pred_fn_def_id, &paths::DEREF_TRAIT_METHOD);
                    if match_type(cx, pred_arg_ty, &paths::PATH_BUF)
                        || match_type(cx, pred_arg_ty, &paths::OS_STRING);
                    then {
                        pred_arg
                    } else {
                        continue;
                    }
                };

                let (local, cannot_move_out) =
                    unwrap_or_continue!(find_stmt_assigns_to(cx, mir, pred_arg, true, ps[0]));
                let loc = mir::Location {
                    block: bb,
                    statement_index: mir.basic_blocks()[bb].statements.len(),
                };

                // This can be turned into `res = move local` if `arg` and `cloned` are not borrowed
                // at the last statement:
                //
                // ```
                // pred_arg = &local;
                // cloned = deref(pred_arg);
                // arg = &cloned;
                // StorageDead(pred_arg);
                // res = to_path_buf(cloned);
                // ```
                if cannot_move_out || !possible_borrower.only_borrowers(&[arg, cloned], local, loc) {
                    continue;
                }

                local
            };

            // `local` cannot be moved out if it is used later
            let used_later = traversal::ReversePostorder::new(&mir, bb).skip(1).any(|(tbb, tdata)| {
                // Give up on loops
                if tdata.terminator().successors().any(|s| *s == bb) {
                    return true;
                }

                let mut vis = LocalUseVisitor {
                    local,
                    used_other_than_drop: false,
                };
                vis.visit_basic_block_data(tbb, tdata);
                vis.used_other_than_drop
            });

            if !used_later {
                let span = terminator.source_info.span;
                let scope = terminator.source_info.scope;
                let node = mir.source_scopes[scope]
                    .local_data
                    .as_ref()
                    .assert_crate_local()
                    .lint_root;

                if_chain! {
                    if let Some(snip) = snippet_opt(cx, span);
                    if let Some(dot) = snip.rfind('.');
                    then {
                        let sugg_span = span.with_lo(
                            span.lo() + BytePos(u32::try_from(dot).unwrap())
                        );
                        let mut app = Applicability::MaybeIncorrect;

                        let mut call_snip = &snip[dot + 1..];
                        // Machine applicable when `call_snip` looks like `foobar()`
                        if call_snip.ends_with("()") {
                            call_snip = call_snip[..call_snip.len()-2].trim();
                            if call_snip.as_bytes().iter().all(|b| b.is_ascii_alphabetic() || *b == b'_') {
                                app = Applicability::MachineApplicable;
                            }
                        }

                        span_lint_hir_and_then(cx, REDUNDANT_CLONE, node, sugg_span, "redundant clone", |db| {
                            db.span_suggestion(
                                sugg_span,
                                "remove this",
                                String::new(),
                                app,
                            );
                            db.span_note(
                                span.with_hi(span.lo() + BytePos(u32::try_from(dot).unwrap())),
                                "this value is dropped without further use",
                            );
                        });
                    } else {
                        span_lint_hir(cx, REDUNDANT_CLONE, node, span, "redundant clone");
                    }
                }
            }
        }
    }
}

/// If `kind` is `y = func(x: &T)` where `T: !Copy`, returns `(DefId of func, x, T, y)`.
fn is_call_with_ref_arg<'tcx>(
    cx: &LateContext<'_, 'tcx>,
    mir: &'tcx mir::Body<'tcx>,
    kind: &'tcx mir::TerminatorKind<'tcx>,
) -> Option<(def_id::DefId, mir::Local, Ty<'tcx>, Option<&'tcx mir::Place<'tcx>>)> {
    if_chain! {
        if let mir::TerminatorKind::Call { func, args, destination, .. } = kind;
        if args.len() == 1;
        if let mir::Operand::Move(mir::Place { base: mir::PlaceBase::Local(local), .. }) = &args[0];
        if let ty::FnDef(def_id, _) = func.ty(&*mir, cx.tcx).kind;
        if let (inner_ty, 1) = walk_ptrs_ty_depth(args[0].ty(&*mir, cx.tcx));
        if !is_copy(cx, inner_ty);
        then {
            Some((def_id, *local, inner_ty, destination.as_ref().map(|(dest, _)| dest)))
        } else {
            None
        }
    }
}

type CannotMoveOut = bool;

/// Finds the first `to = (&)from`, and returns
/// ``Some((from, whether `from` cannot be moved out))``.
fn find_stmt_assigns_to<'tcx>(
    cx: &LateContext<'_, 'tcx>,
    mir: &mir::Body<'tcx>,
    to_local: mir::Local,
    by_ref: bool,
    bb: mir::BasicBlock,
) -> Option<(mir::Local, CannotMoveOut)> {
    let rvalue = mir.basic_blocks()[bb].statements.iter().rev().find_map(|stmt| {
        if let mir::StatementKind::Assign(box (
            mir::Place {
                base: mir::PlaceBase::Local(local),
                ..
            },
            v,
        )) = &stmt.kind
        {
            return if *local == to_local { Some(v) } else { None };
        }

        None
    })?;

    match (by_ref, &*rvalue) {
        (true, mir::Rvalue::Ref(_, _, place)) | (false, mir::Rvalue::Use(mir::Operand::Copy(place))) => {
            base_local_and_movability(cx, mir, place)
        },
        _ => None,
    }
}

/// Extracts and returns the undermost base `Local` of given `place`. Returns `place` itself
/// if it is already a `Local`.
///
/// Also reports whether given `place` cannot be moved out.
fn base_local_and_movability<'tcx>(
    cx: &LateContext<'_, 'tcx>,
    mir: &mir::Body<'tcx>,
    place: &mir::Place<'tcx>,
) -> Option<(mir::Local, CannotMoveOut)> {
    use rustc::mir::PlaceRef;

    // Dereference. You cannot move things out from a borrowed value.
    let mut deref = false;
    // Accessing a field of an ADT that has `Drop`. Moving the field out will cause E0509.
    let mut field = false;

    let PlaceRef {
        base: place_base,
        mut projection,
    } = place.as_ref();
    if let mir::PlaceBase::Local(local) = place_base {
        while let [base @ .., elem] = projection {
            projection = base;
            deref |= matches!(elem, mir::ProjectionElem::Deref);
            field |= matches!(elem, mir::ProjectionElem::Field(..))
                && has_drop(
                    cx,
                    mir::Place::ty_from(place_base, projection, &mir.local_decls, cx.tcx).ty,
                );
        }

        Some((*local, deref || field))
    } else {
        None
    }
}

struct LocalUseVisitor {
    local: mir::Local,
    used_other_than_drop: bool,
}

impl<'tcx> mir::visit::Visitor<'tcx> for LocalUseVisitor {
    fn visit_basic_block_data(&mut self, block: mir::BasicBlock, data: &mir::BasicBlockData<'tcx>) {
        let statements = &data.statements;
        for (statement_index, statement) in statements.iter().enumerate() {
            self.visit_statement(statement, mir::Location { block, statement_index });

            // Once flagged, skip remaining statements
            if self.used_other_than_drop {
                return;
            }
        }

        self.visit_terminator(
            data.terminator(),
            mir::Location {
                block,
                statement_index: statements.len(),
            },
        );
    }

    fn visit_local(&mut self, local: &mir::Local, ctx: PlaceContext, _: mir::Location) {
        match ctx {
            PlaceContext::MutatingUse(MutatingUseContext::Drop) | PlaceContext::NonUse(_) => return,
            _ => {},
        }

        if *local == self.local {
            self.used_other_than_drop = true;
        }
    }
}

/// Determines liveness of each local purely based on `StorageLive`/`Dead`.
#[derive(Copy, Clone)]
struct MaybeStorageLive<'a, 'tcx> {
    body: &'a mir::Body<'tcx>,
}

impl<'a, 'tcx> MaybeStorageLive<'a, 'tcx> {
    fn new(body: &'a mir::Body<'tcx>) -> Self {
        MaybeStorageLive { body }
    }
}

impl<'a, 'tcx> BitDenotation<'tcx> for MaybeStorageLive<'a, 'tcx> {
    type Idx = mir::Local;
    fn name() -> &'static str {
        "maybe_storage_live"
    }
    fn bits_per_block(&self) -> usize {
        self.body.local_decls.len()
    }

    fn start_block_effect(&self, on_entry: &mut BitSet<mir::Local>) {
        for arg in self.body.args_iter() {
            on_entry.insert(arg);
        }
    }

    fn statement_effect(&self, trans: &mut GenKillSet<mir::Local>, loc: mir::Location) {
        let stmt = &self.body[loc.block].statements[loc.statement_index];

        match stmt.kind {
            mir::StatementKind::StorageLive(l) => trans.gen(l),
            mir::StatementKind::StorageDead(l) => trans.kill(l),
            _ => (),
        }
    }

    fn terminator_effect(&self, _trans: &mut GenKillSet<mir::Local>, _loc: mir::Location) {}

    fn propagate_call_return(
        &self,
        _in_out: &mut BitSet<mir::Local>,
        _call_bb: mir::BasicBlock,
        _dest_bb: mir::BasicBlock,
        _dest_place: &mir::Place<'tcx>,
    ) {
        // Nothing to do when a call returns successfully
    }
}

impl<'a, 'tcx> BottomValue for MaybeStorageLive<'a, 'tcx> {
    /// bottom = dead
    const BOTTOM_VALUE: bool = false;
}

/// Collects the possible borrowers of each local.
/// For example, `b = &a; c = &a;` will make `b` and (transitively) `c`
/// possible borrowers of `a`.
struct PossibleBorrowerVisitor<'a, 'tcx> {
    possible_borrower: TransitiveRelation<mir::Local>,
    body: &'a mir::Body<'tcx>,
    cx: &'a LateContext<'a, 'tcx>,
}

impl<'a, 'tcx> PossibleBorrowerVisitor<'a, 'tcx> {
    fn new(cx: &'a LateContext<'a, 'tcx>, body: &'a mir::Body<'tcx>) -> Self {
        Self {
            possible_borrower: TransitiveRelation::default(),
            cx,
            body,
        }
    }

    fn into_map(
        self,
        cx: &LateContext<'a, 'tcx>,
        maybe_live: DataflowResults<'tcx, MaybeStorageLive<'a, 'tcx>>,
    ) -> PossibleBorrower<'a, 'tcx> {
        let mut map = FxHashMap::default();
        for row in (1..self.body.local_decls.len()).map(mir::Local::from_usize) {
            if is_copy(cx, self.body.local_decls[row].ty) {
                continue;
            }

            let borrowers = self.possible_borrower.reachable_from(&row);
            if !borrowers.is_empty() {
                let mut bs = HybridBitSet::new_empty(self.body.local_decls.len());
                for &c in borrowers {
                    if c != mir::Local::from_usize(0) {
                        bs.insert(c);
                    }
                }

                if !bs.is_empty() {
                    map.insert(row, bs);
                }
            }
        }

        let bs = BitSet::new_empty(self.body.local_decls.len());
        PossibleBorrower {
            map,
            maybe_live: DataflowResultsCursor::new(maybe_live, self.body),
            bitset: (bs.clone(), bs),
        }
    }
}

impl<'a, 'tcx> mir::visit::Visitor<'tcx> for PossibleBorrowerVisitor<'a, 'tcx> {
    fn visit_assign(&mut self, place: &mir::Place<'tcx>, rvalue: &mir::Rvalue<'_>, _location: mir::Location) {
        if let mir::PlaceBase::Local(lhs) = place.base {
            match rvalue {
                mir::Rvalue::Ref(_, _, borrowed) => {
                    if let mir::PlaceBase::Local(borrowed_local) = borrowed.base {
                        self.possible_borrower.add(borrowed_local, lhs);
                    }
                },
                other => {
                    if !ContainsRegion.visit_ty(place.ty(&self.body.local_decls, self.cx.tcx).ty) {
                        return;
                    }
                    rvalue_locals(other, |rhs| {
                        if lhs != rhs {
                            self.possible_borrower.add(rhs, lhs);
                        }
                    });
                },
            }
        }
    }

    fn visit_terminator(&mut self, terminator: &mir::Terminator<'_>, _loc: mir::Location) {
        if let mir::TerminatorKind::Call {
            args,
            destination:
                Some((
                    mir::Place {
                        base: mir::PlaceBase::Local(dest),
                        ..
                    },
                    _,
                )),
            ..
        } = &terminator.kind
        {
            // If the call returns something with lifetimes,
            // let's conservatively assume the returned value contains lifetime of all the arguments.
            // For example, given `let y: Foo<'a> = foo(x)`, `y` is considered to be a possible borrower of `x`.
            if !ContainsRegion.visit_ty(&self.body.local_decls[*dest].ty) {
                return;
            }

            for op in args {
                match op {
                    mir::Operand::Copy(p) | mir::Operand::Move(p) => {
                        if let mir::PlaceBase::Local(arg) = p.base {
                            self.possible_borrower.add(arg, *dest);
                        }
                    },
                    _ => (),
                }
            }
        }
    }
}

struct ContainsRegion;

impl TypeVisitor<'_> for ContainsRegion {
    fn visit_region(&mut self, _: ty::Region<'_>) -> bool {
        true
    }
}

fn rvalue_locals(rvalue: &mir::Rvalue<'_>, mut visit: impl FnMut(mir::Local)) {
    use rustc::mir::Rvalue::*;

    let mut visit_op = |op: &mir::Operand<'_>| match op {
        mir::Operand::Copy(p) | mir::Operand::Move(p) => {
            if let mir::PlaceBase::Local(l) = p.base {
                visit(l)
            }
        },
        _ => (),
    };

    match rvalue {
        Use(op) | Repeat(op, _) | Cast(_, op, _) | UnaryOp(_, op) => visit_op(op),
        Aggregate(_, ops) => ops.iter().for_each(visit_op),
        BinaryOp(_, lhs, rhs) | CheckedBinaryOp(_, lhs, rhs) => {
            visit_op(lhs);
            visit_op(rhs);
        },
        _ => (),
    }
}

/// Result of `PossibleBorrowerVisitor`.
struct PossibleBorrower<'a, 'tcx> {
    /// Mapping `Local -> its possible borrowers`
    map: FxHashMap<mir::Local, HybridBitSet<mir::Local>>,
    maybe_live: DataflowResultsCursor<'a, 'tcx, MaybeStorageLive<'a, 'tcx>>,
    // Caches to avoid allocation of `BitSet` on every query
    bitset: (BitSet<mir::Local>, BitSet<mir::Local>),
}

impl PossibleBorrower<'_, '_> {
    /// Returns true if the set of borrowers of `borrowed` living at `at` matches with `borrowers`.
    fn only_borrowers(&mut self, borrowers: &[mir::Local], borrowed: mir::Local, at: mir::Location) -> bool {
        self.maybe_live.seek(at);

        self.bitset.0.clear();
        let maybe_live = &mut self.maybe_live;
        if let Some(bitset) = self.map.get(&borrowed) {
            for b in bitset.iter().filter(move |b| maybe_live.contains(*b)) {
                self.bitset.0.insert(b);
            }
        } else {
            return false;
        }

        self.bitset.1.clear();
        for b in borrowers {
            self.bitset.1.insert(*b);
        }

        self.bitset.0 == self.bitset.1
    }
}