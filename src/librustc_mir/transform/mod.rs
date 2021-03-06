use crate::{shim, util};
use required_consts::RequiredConstsVisitor;
use rustc_data_structures::fx::FxHashSet;
use rustc_hir as hir;
use rustc_hir::def_id::{CrateNum, DefId, LocalDefId, LOCAL_CRATE};
use rustc_hir::intravisit::{self, NestedVisitorMap, Visitor};
use rustc_index::vec::IndexVec;
use rustc_middle::mir::visit::Visitor as _;
use rustc_middle::mir::{traversal, Body, ConstQualifs, MirPhase, Promoted};
use rustc_middle::ty::query::Providers;
use rustc_middle::ty::steal::Steal;
use rustc_middle::ty::{self, InstanceDef, TyCtxt, TypeFoldable};
use rustc_span::{Span, Symbol};
use std::borrow::Cow;

pub mod add_call_guards;
pub mod add_moves_for_packed_drops;
pub mod add_retag;
pub mod check_consts;
pub mod check_packed_ref;
pub mod check_unsafety;
pub mod cleanup_post_borrowck;
pub mod const_prop;
pub mod copy_prop;
pub mod deaggregator;
pub mod dump_mir;
pub mod elaborate_drops;
pub mod generator;
pub mod inline;
pub mod instcombine;
pub mod instrument_coverage;
pub mod no_landing_pads;
pub mod nrvo;
pub mod promote_consts;
pub mod qualify_min_const_fn;
pub mod remove_noop_landing_pads;
pub mod required_consts;
pub mod rustc_peek;
pub mod simplify;
pub mod simplify_branches;
pub mod simplify_try;
pub mod uninhabited_enum_branching;
pub mod unreachable_prop;
pub mod validate;

pub(crate) fn provide(providers: &mut Providers) {
    self::check_unsafety::provide(providers);
    *providers = Providers {
        mir_keys,
        mir_const,
        mir_const_qualif: |tcx, def_id| {
            let def_id = def_id.expect_local();
            if let Some(def) = ty::WithOptConstParam::try_lookup(def_id, tcx) {
                tcx.mir_const_qualif_const_arg(def)
            } else {
                mir_const_qualif(tcx, ty::WithOptConstParam::unknown(def_id))
            }
        },
        mir_const_qualif_const_arg: |tcx, (did, param_did)| {
            mir_const_qualif(tcx, ty::WithOptConstParam { did, const_param_did: Some(param_did) })
        },
        mir_validated,
        mir_drops_elaborated_and_const_checked,
        optimized_mir,
        optimized_mir_of_const_arg,
        is_mir_available,
        promoted_mir: |tcx, def_id| {
            let def_id = def_id.expect_local();
            if let Some(def) = ty::WithOptConstParam::try_lookup(def_id, tcx) {
                tcx.promoted_mir_of_const_arg(def)
            } else {
                promoted_mir(tcx, ty::WithOptConstParam::unknown(def_id))
            }
        },
        promoted_mir_of_const_arg: |tcx, (did, param_did)| {
            promoted_mir(tcx, ty::WithOptConstParam { did, const_param_did: Some(param_did) })
        },
        ..*providers
    };
    instrument_coverage::provide(providers);
}

fn is_mir_available(tcx: TyCtxt<'_>, def_id: DefId) -> bool {
    tcx.mir_keys(def_id.krate).contains(&def_id.expect_local())
}

/// Finds the full set of `DefId`s within the current crate that have
/// MIR associated with them.
fn mir_keys(tcx: TyCtxt<'_>, krate: CrateNum) -> FxHashSet<LocalDefId> {
    assert_eq!(krate, LOCAL_CRATE);

    let mut set = FxHashSet::default();

    // All body-owners have MIR associated with them.
    set.extend(tcx.body_owners());

    // Additionally, tuple struct/variant constructors have MIR, but
    // they don't have a BodyId, so we need to build them separately.
    struct GatherCtors<'a, 'tcx> {
        tcx: TyCtxt<'tcx>,
        set: &'a mut FxHashSet<LocalDefId>,
    }
    impl<'a, 'tcx> Visitor<'tcx> for GatherCtors<'a, 'tcx> {
        fn visit_variant_data(
            &mut self,
            v: &'tcx hir::VariantData<'tcx>,
            _: Symbol,
            _: &'tcx hir::Generics<'tcx>,
            _: hir::HirId,
            _: Span,
        ) {
            if let hir::VariantData::Tuple(_, hir_id) = *v {
                self.set.insert(self.tcx.hir().local_def_id(hir_id));
            }
            intravisit::walk_struct_def(self, v)
        }
        type Map = intravisit::ErasedMap<'tcx>;
        fn nested_visit_map(&mut self) -> NestedVisitorMap<Self::Map> {
            NestedVisitorMap::None
        }
    }
    tcx.hir()
        .krate()
        .visit_all_item_likes(&mut GatherCtors { tcx, set: &mut set }.as_deep_visitor());

    set
}

/// Where a specific `mir::Body` comes from.
#[derive(Debug, Copy, Clone)]
pub struct MirSource<'tcx> {
    pub instance: InstanceDef<'tcx>,

    /// If `Some`, this is a promoted rvalue within the parent function.
    pub promoted: Option<Promoted>,
}

impl<'tcx> MirSource<'tcx> {
    pub fn item(def_id: DefId) -> Self {
        MirSource {
            instance: InstanceDef::Item(ty::WithOptConstParam::unknown(def_id)),
            promoted: None,
        }
    }

    pub fn with_opt_param(self) -> ty::WithOptConstParam<DefId> {
        self.instance.with_opt_param()
    }

    #[inline]
    pub fn def_id(&self) -> DefId {
        self.instance.def_id()
    }
}

/// Generates a default name for the pass based on the name of the
/// type `T`.
pub fn default_name<T: ?Sized>() -> Cow<'static, str> {
    let name = ::std::any::type_name::<T>();
    if let Some(tail) = name.rfind(':') { Cow::from(&name[tail + 1..]) } else { Cow::from(name) }
}

/// A streamlined trait that you can implement to create a pass; the
/// pass will be named after the type, and it will consist of a main
/// loop that goes over each available MIR and applies `run_pass`.
pub trait MirPass<'tcx> {
    fn name(&self) -> Cow<'_, str> {
        default_name::<Self>()
    }

    fn run_pass(&self, tcx: TyCtxt<'tcx>, source: MirSource<'tcx>, body: &mut Body<'tcx>);
}

pub fn run_passes(
    tcx: TyCtxt<'tcx>,
    body: &mut Body<'tcx>,
    instance: InstanceDef<'tcx>,
    promoted: Option<Promoted>,
    mir_phase: MirPhase,
    passes: &[&[&dyn MirPass<'tcx>]],
) {
    let phase_index = mir_phase.phase_index();
    let source = MirSource { instance, promoted };
    let validate = tcx.sess.opts.debugging_opts.validate_mir;

    if body.phase >= mir_phase {
        return;
    }

    if validate {
        validate::Validator { when: format!("input to phase {:?}", mir_phase) }
            .run_pass(tcx, source, body);
    }

    let mut index = 0;
    let mut run_pass = |pass: &dyn MirPass<'tcx>| {
        let run_hooks = |body: &_, index, is_after| {
            dump_mir::on_mir_pass(
                tcx,
                &format_args!("{:03}-{:03}", phase_index, index),
                &pass.name(),
                source,
                body,
                is_after,
            );
        };
        run_hooks(body, index, false);
        pass.run_pass(tcx, source, body);
        run_hooks(body, index, true);

        if validate {
            validate::Validator { when: format!("after {} in phase {:?}", pass.name(), mir_phase) }
                .run_pass(tcx, source, body);
        }

        index += 1;
    };

    for pass_group in passes {
        for pass in *pass_group {
            run_pass(*pass);
        }
    }

    body.phase = mir_phase;

    if mir_phase == MirPhase::Optimized {
        validate::Validator { when: format!("end of phase {:?}", mir_phase) }
            .run_pass(tcx, source, body);
    }
}

fn mir_const_qualif(tcx: TyCtxt<'_>, def: ty::WithOptConstParam<LocalDefId>) -> ConstQualifs {
    let const_kind = tcx.hir().body_const_context(def.did);

    // No need to const-check a non-const `fn`.
    if const_kind.is_none() {
        return Default::default();
    }

    // N.B., this `borrow()` is guaranteed to be valid (i.e., the value
    // cannot yet be stolen), because `mir_validated()`, which steals
    // from `mir_const(), forces this query to execute before
    // performing the steal.
    let body = &tcx.mir_const(def).borrow();

    if body.return_ty().references_error() {
        tcx.sess.delay_span_bug(body.span, "mir_const_qualif: MIR had errors");
        return Default::default();
    }

    let ccx = check_consts::ConstCx {
        body,
        tcx,
        def_id: def.did,
        const_kind,
        param_env: tcx.param_env(def.did),
    };

    let mut validator = check_consts::validation::Validator::new(&ccx);
    validator.check_body();

    // We return the qualifs in the return place for every MIR body, even though it is only used
    // when deciding to promote a reference to a `const` for now.
    validator.qualifs_in_return_place()
}

/// Make MIR ready for const evaluation. This is run on all MIR, not just on consts!
fn mir_const<'tcx>(
    tcx: TyCtxt<'tcx>,
    def: ty::WithOptConstParam<LocalDefId>,
) -> &'tcx Steal<Body<'tcx>> {
    if let Some(def) = def.try_upgrade(tcx) {
        return tcx.mir_const(def);
    }

    // Unsafety check uses the raw mir, so make sure it is run.
    if let Some(param_did) = def.const_param_did {
        tcx.ensure().unsafety_check_result_for_const_arg((def.did, param_did));
    } else {
        tcx.ensure().unsafety_check_result(def.did);
    }

    let mut body = tcx.mir_built(def).steal();

    util::dump_mir(
        tcx,
        None,
        "mir_map",
        &0,
        MirSource { instance: InstanceDef::Item(def.to_global()), promoted: None },
        &body,
        |_, _| Ok(()),
    );

    run_passes(
        tcx,
        &mut body,
        InstanceDef::Item(def.to_global()),
        None,
        MirPhase::Const,
        &[&[
            // MIR-level lints.
            &check_packed_ref::CheckPackedRef,
            // What we need to do constant evaluation.
            &simplify::SimplifyCfg::new("initial"),
            &rustc_peek::SanityCheck,
        ]],
    );
    tcx.alloc_steal_mir(body)
}

fn mir_validated(
    tcx: TyCtxt<'tcx>,
    def: ty::WithOptConstParam<LocalDefId>,
) -> (&'tcx Steal<Body<'tcx>>, &'tcx Steal<IndexVec<Promoted, Body<'tcx>>>) {
    if let Some(def) = def.try_upgrade(tcx) {
        return tcx.mir_validated(def);
    }

    // Ensure that we compute the `mir_const_qualif` for constants at
    // this point, before we steal the mir-const result.
    let _ = tcx.mir_const_qualif_opt_const_arg(def);

    let mut body = tcx.mir_const(def).steal();

    let mut required_consts = Vec::new();
    let mut required_consts_visitor = RequiredConstsVisitor::new(&mut required_consts);
    for (bb, bb_data) in traversal::reverse_postorder(&body) {
        required_consts_visitor.visit_basic_block_data(bb, bb_data);
    }
    body.required_consts = required_consts;

    let promote_pass = promote_consts::PromoteTemps::default();
    let promote: &[&dyn MirPass<'tcx>] = &[
        // What we need to run borrowck etc.
        &promote_pass,
        &simplify::SimplifyCfg::new("qualify-consts"),
    ];

    let opt_coverage: &[&dyn MirPass<'tcx>] = if tcx.sess.opts.debugging_opts.instrument_coverage {
        &[&instrument_coverage::InstrumentCoverage]
    } else {
        &[]
    };

    run_passes(
        tcx,
        &mut body,
        InstanceDef::Item(def.to_global()),
        None,
        MirPhase::Validated,
        &[promote, opt_coverage],
    );

    let promoted = promote_pass.promoted_fragments.into_inner();
    (tcx.alloc_steal_mir(body), tcx.alloc_steal_promoted(promoted))
}

fn mir_drops_elaborated_and_const_checked<'tcx>(
    tcx: TyCtxt<'tcx>,
    def: ty::WithOptConstParam<LocalDefId>,
) -> &'tcx Steal<Body<'tcx>> {
    if let Some(def) = def.try_upgrade(tcx) {
        return tcx.mir_drops_elaborated_and_const_checked(def);
    }

    // (Mir-)Borrowck uses `mir_validated`, so we have to force it to
    // execute before we can steal.
    if let Some(param_did) = def.const_param_did {
        tcx.ensure().mir_borrowck_const_arg((def.did, param_did));
    } else {
        tcx.ensure().mir_borrowck(def.did);
    }

    let (body, _) = tcx.mir_validated(def);
    let mut body = body.steal();

    run_post_borrowck_cleanup_passes(tcx, &mut body, def.did, None);
    check_consts::post_drop_elaboration::check_live_drops(tcx, def.did, &body);
    tcx.alloc_steal_mir(body)
}

/// After this series of passes, no lifetime analysis based on borrowing can be done.
fn run_post_borrowck_cleanup_passes<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &mut Body<'tcx>,
    def_id: LocalDefId,
    promoted: Option<Promoted>,
) {
    debug!("post_borrowck_cleanup({:?})", def_id);

    let post_borrowck_cleanup: &[&dyn MirPass<'tcx>] = &[
        // Remove all things only needed by analysis
        &no_landing_pads::NoLandingPads::new(tcx),
        &simplify_branches::SimplifyBranches::new("initial"),
        &remove_noop_landing_pads::RemoveNoopLandingPads,
        &cleanup_post_borrowck::CleanupNonCodegenStatements,
        &simplify::SimplifyCfg::new("early-opt"),
        // These next passes must be executed together
        &add_call_guards::CriticalCallEdges,
        &elaborate_drops::ElaborateDrops,
        &no_landing_pads::NoLandingPads::new(tcx),
        // AddMovesForPackedDrops needs to run after drop
        // elaboration.
        &add_moves_for_packed_drops::AddMovesForPackedDrops,
        // `AddRetag` needs to run after `ElaborateDrops`. Otherwise it should run fairly late,
        // but before optimizations begin.
        &add_retag::AddRetag,
        &simplify::SimplifyCfg::new("elaborate-drops"),
    ];

    run_passes(
        tcx,
        body,
        InstanceDef::Item(ty::WithOptConstParam::unknown(def_id.to_def_id())),
        promoted,
        MirPhase::DropElab,
        &[post_borrowck_cleanup],
    );
}

fn run_optimization_passes<'tcx>(
    tcx: TyCtxt<'tcx>,
    body: &mut Body<'tcx>,
    def_id: LocalDefId,
    promoted: Option<Promoted>,
) {
    let optimizations: &[&dyn MirPass<'tcx>] = &[
        &unreachable_prop::UnreachablePropagation,
        &uninhabited_enum_branching::UninhabitedEnumBranching,
        &simplify::SimplifyCfg::new("after-uninhabited-enum-branching"),
        &inline::Inline,
        // Lowering generator control-flow and variables has to happen before we do anything else
        // to them. We do this inside the "optimizations" block so that it can benefit from
        // optimizations that run before, that might be harder to do on the state machine than MIR
        // with async primitives.
        &generator::StateTransform,
        &instcombine::InstCombine,
        &const_prop::ConstProp,
        &simplify_branches::SimplifyBranches::new("after-const-prop"),
        // Run deaggregation here because:
        //   1. Some codegen backends require it
        //   2. It creates additional possibilities for some MIR optimizations to trigger
        // FIXME(#70073): Why is this done here and not in `post_borrowck_cleanup`?
        &deaggregator::Deaggregator,
        &simplify_try::SimplifyArmIdentity,
        &simplify_try::SimplifyBranchSame,
        &copy_prop::CopyPropagation,
        &simplify_branches::SimplifyBranches::new("after-copy-prop"),
        &remove_noop_landing_pads::RemoveNoopLandingPads,
        &simplify::SimplifyCfg::new("after-remove-noop-landing-pads"),
        &simplify::SimplifyCfg::new("final"),
        &nrvo::RenameReturnPlace,
        &simplify::SimplifyLocals,
    ];

    let no_optimizations: &[&dyn MirPass<'tcx>] = &[
        // Even if we don't do optimizations, we still have to lower generators for codegen.
        &generator::StateTransform,
        // FIXME(#70073): This pass is responsible for both optimization as well as some lints.
        &const_prop::ConstProp,
        // Even if we don't do optimizations, still run deaggregation because some backends assume
        // that deaggregation always occurs.
        &deaggregator::Deaggregator,
    ];

    let pre_codegen_cleanup: &[&dyn MirPass<'tcx>] = &[
        &add_call_guards::CriticalCallEdges,
        // Dump the end result for testing and debugging purposes.
        &dump_mir::Marker("PreCodegen"),
    ];

    let mir_opt_level = tcx.sess.opts.debugging_opts.mir_opt_level;

    #[rustfmt::skip]
    run_passes(
        tcx,
        body,
        InstanceDef::Item(ty::WithOptConstParam::unknown(def_id.to_def_id())),
        promoted,
        MirPhase::Optimized,
        &[
            if mir_opt_level > 0 { optimizations } else { no_optimizations },
            pre_codegen_cleanup,
        ],
    );
}

fn optimized_mir<'tcx>(tcx: TyCtxt<'tcx>, did: DefId) -> &'tcx Body<'tcx> {
    let did = did.expect_local();
    if let Some(def) = ty::WithOptConstParam::try_lookup(did, tcx) {
        tcx.optimized_mir_of_const_arg(def)
    } else {
        tcx.arena.alloc(inner_optimized_mir(tcx, ty::WithOptConstParam::unknown(did)))
    }
}

fn optimized_mir_of_const_arg<'tcx>(
    tcx: TyCtxt<'tcx>,
    (did, param_did): (LocalDefId, DefId),
) -> &'tcx Body<'tcx> {
    tcx.arena.alloc(inner_optimized_mir(
        tcx,
        ty::WithOptConstParam { did, const_param_did: Some(param_did) },
    ))
}

fn inner_optimized_mir(tcx: TyCtxt<'_>, def: ty::WithOptConstParam<LocalDefId>) -> Body<'_> {
    if tcx.is_constructor(def.did.to_def_id()) {
        // There's no reason to run all of the MIR passes on constructors when
        // we can just output the MIR we want directly. This also saves const
        // qualification and borrow checking the trouble of special casing
        // constructors.
        return shim::build_adt_ctor(tcx, def.did.to_def_id());
    }

    let mut body = tcx.mir_drops_elaborated_and_const_checked(def).steal();
    run_optimization_passes(tcx, &mut body, def.did, None);

    debug_assert!(!body.has_free_regions(), "Free regions in optimized MIR");

    body
}

fn promoted_mir<'tcx>(
    tcx: TyCtxt<'tcx>,
    def: ty::WithOptConstParam<LocalDefId>,
) -> &'tcx IndexVec<Promoted, Body<'tcx>> {
    if tcx.is_constructor(def.did.to_def_id()) {
        return tcx.arena.alloc(IndexVec::new());
    }

    if let Some(param_did) = def.const_param_did {
        tcx.ensure().mir_borrowck_const_arg((def.did, param_did));
    } else {
        tcx.ensure().mir_borrowck(def.did);
    }
    let (_, promoted) = tcx.mir_validated(def);
    let mut promoted = promoted.steal();

    for (p, mut body) in promoted.iter_enumerated_mut() {
        run_post_borrowck_cleanup_passes(tcx, &mut body, def.did, Some(p));
        run_optimization_passes(tcx, &mut body, def.did, Some(p));
    }

    debug_assert!(!promoted.has_free_regions(), "Free regions in promoted MIR");

    tcx.arena.alloc(promoted)
}
