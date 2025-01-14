use std::collections::VecDeque;
use std::rc::Rc;

use crate::borrow_check::nll::region_infer::{Cause, RegionInferenceContext};
use crate::borrow_check::nll::ToRegionVid;
use crate::util::liveness::{self, DefUse};
use rustc::mir::visit::{MirVisitable, PlaceContext, Visitor};
use rustc::mir::{Local, Location, Body};
use rustc::ty::{RegionVid, TyCtxt};
use rustc_data_structures::fx::FxHashSet;

crate fn find<'tcx>(
    body: &Body<'tcx>,
    regioncx: &Rc<RegionInferenceContext<'tcx>>,
    tcx: TyCtxt<'_, 'tcx>,
    region_vid: RegionVid,
    start_point: Location,
) -> Option<Cause> {
    let mut uf = UseFinder {
        body,
        regioncx,
        tcx,
        region_vid,
        start_point,
    };

    uf.find()
}

struct UseFinder<'cx, 'gcx: 'tcx, 'tcx: 'cx> {
    body: &'cx Body<'tcx>,
    regioncx: &'cx Rc<RegionInferenceContext<'tcx>>,
    tcx: TyCtxt<'gcx, 'tcx>,
    region_vid: RegionVid,
    start_point: Location,
}

impl<'cx, 'gcx, 'tcx> UseFinder<'cx, 'gcx, 'tcx> {
    fn find(&mut self) -> Option<Cause> {
        let mut queue = VecDeque::new();
        let mut visited = FxHashSet::default();

        queue.push_back(self.start_point);
        while let Some(p) = queue.pop_front() {
            if !self.regioncx.region_contains(self.region_vid, p) {
                continue;
            }

            if !visited.insert(p) {
                continue;
            }

            let block_data = &self.body[p.block];

            match self.def_use(p, block_data.visitable(p.statement_index)) {
                Some(DefUseResult::Def) => {}

                Some(DefUseResult::UseLive { local }) => {
                    return Some(Cause::LiveVar(local, p));
                }

                Some(DefUseResult::UseDrop { local }) => {
                    return Some(Cause::DropVar(local, p));
                }

                None => {
                    if p.statement_index < block_data.statements.len() {
                        queue.push_back(p.successor_within_block());
                    } else {
                        queue.extend(
                            block_data
                                .terminator()
                                .successors()
                                .filter(|&bb| Some(&Some(*bb)) != block_data.terminator().unwind())
                                .map(|&bb| Location {
                                    statement_index: 0,
                                    block: bb,
                                }),
                        );
                    }
                }
            }
        }

        None
    }

    fn def_use(&self, location: Location, thing: &dyn MirVisitable<'tcx>) -> Option<DefUseResult> {
        let mut visitor = DefUseVisitor {
            body: self.body,
            tcx: self.tcx,
            region_vid: self.region_vid,
            def_use_result: None,
        };

        thing.apply(location, &mut visitor);

        visitor.def_use_result
    }
}

struct DefUseVisitor<'cx, 'gcx: 'tcx, 'tcx: 'cx> {
    body: &'cx Body<'tcx>,
    tcx: TyCtxt<'gcx, 'tcx>,
    region_vid: RegionVid,
    def_use_result: Option<DefUseResult>,
}

enum DefUseResult {
    Def,
    UseLive { local: Local },
    UseDrop { local: Local },
}

impl<'cx, 'gcx, 'tcx> Visitor<'tcx> for DefUseVisitor<'cx, 'gcx, 'tcx> {
    fn visit_local(&mut self, &local: &Local, context: PlaceContext, _: Location) {
        let local_ty = self.body.local_decls[local].ty;

        let mut found_it = false;
        self.tcx.for_each_free_region(&local_ty, |r| {
            if r.to_region_vid() == self.region_vid {
                found_it = true;
            }
        });

        if found_it {
            self.def_use_result = match liveness::categorize(context) {
                Some(DefUse::Def) => Some(DefUseResult::Def),
                Some(DefUse::Use) => Some(DefUseResult::UseLive { local }),
                Some(DefUse::Drop) => Some(DefUseResult::UseDrop { local }),
                None => None,
            };
        }
    }
}
