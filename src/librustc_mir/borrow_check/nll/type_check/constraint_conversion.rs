use crate::borrow_check::nll::constraints::OutlivesConstraint;
use crate::borrow_check::nll::region_infer::TypeTest;
use crate::borrow_check::nll::type_check::{Locations, MirTypeckRegionConstraints};
use crate::borrow_check::nll::universal_regions::UniversalRegions;
use crate::borrow_check::nll::ToRegionVid;
use rustc::infer::canonical::QueryRegionConstraint;
use rustc::infer::outlives::env::RegionBoundPairs;
use rustc::infer::outlives::obligations::{TypeOutlives, TypeOutlivesDelegate};
use rustc::infer::region_constraints::{GenericKind, VerifyBound};
use rustc::infer::{self, InferCtxt, SubregionOrigin};
use rustc::mir::ConstraintCategory;
use rustc::ty::subst::UnpackedKind;
use rustc::ty::{self, TyCtxt};
use syntax_pos::DUMMY_SP;

crate struct ConstraintConversion<'a, 'gcx: 'tcx, 'tcx: 'a> {
    infcx: &'a InferCtxt<'a, 'gcx, 'tcx>,
    tcx: TyCtxt<'gcx, 'tcx>,
    universal_regions: &'a UniversalRegions<'tcx>,
    region_bound_pairs: &'a RegionBoundPairs<'tcx>,
    implicit_region_bound: Option<ty::Region<'tcx>>,
    param_env: ty::ParamEnv<'tcx>,
    locations: Locations,
    category: ConstraintCategory,
    constraints: &'a mut MirTypeckRegionConstraints<'tcx>,
}

impl<'a, 'gcx, 'tcx> ConstraintConversion<'a, 'gcx, 'tcx> {
    crate fn new(
        infcx: &'a InferCtxt<'a, 'gcx, 'tcx>,
        universal_regions: &'a UniversalRegions<'tcx>,
        region_bound_pairs: &'a RegionBoundPairs<'tcx>,
        implicit_region_bound: Option<ty::Region<'tcx>>,
        param_env: ty::ParamEnv<'tcx>,
        locations: Locations,
        category: ConstraintCategory,
        constraints: &'a mut MirTypeckRegionConstraints<'tcx>,
    ) -> Self {
        Self {
            infcx,
            tcx: infcx.tcx,
            universal_regions,
            region_bound_pairs,
            implicit_region_bound,
            param_env,
            locations,
            category,
            constraints,
        }
    }

    pub(super) fn convert_all(&mut self, query_constraints: &[QueryRegionConstraint<'tcx>]) {
        for query_constraint in query_constraints {
            self.convert(query_constraint);
        }
    }

    pub(super) fn convert(&mut self, query_constraint: &QueryRegionConstraint<'tcx>) {
        debug!("generate: constraints at: {:#?}", self.locations);

        // Extract out various useful fields we'll need below.
        let ConstraintConversion {
            tcx,
            region_bound_pairs,
            implicit_region_bound,
            param_env,
            ..
        } = *self;

        // At the moment, we never generate any "higher-ranked"
        // region constraints like `for<'a> 'a: 'b`. At some point
        // when we move to universes, we will, and this assertion
        // will start to fail.
        let ty::OutlivesPredicate(k1, r2) =
            query_constraint.no_bound_vars().unwrap_or_else(|| {
                bug!(
                    "query_constraint {:?} contained bound vars",
                    query_constraint,
                );
            });

        match k1.unpack() {
            UnpackedKind::Lifetime(r1) => {
                let r1_vid = self.to_region_vid(r1);
                let r2_vid = self.to_region_vid(r2);
                self.add_outlives(r1_vid, r2_vid);
            }

            UnpackedKind::Type(t1) => {
                // we don't actually use this for anything, but
                // the `TypeOutlives` code needs an origin.
                let origin = infer::RelateParamBound(DUMMY_SP, t1);

                TypeOutlives::new(
                    &mut *self,
                    tcx,
                    region_bound_pairs,
                    implicit_region_bound,
                    param_env,
                ).type_must_outlive(origin, t1, r2);
            }

            UnpackedKind::Const(_) => {
                // Consts cannot outlive one another, so we
                // don't need to handle any relations here.
            }
        }
    }

    fn verify_to_type_test(
        &mut self,
        generic_kind: GenericKind<'tcx>,
        region: ty::Region<'tcx>,
        verify_bound: VerifyBound<'tcx>,
    ) -> TypeTest<'tcx> {
        let lower_bound = self.to_region_vid(region);

        TypeTest {
            generic_kind,
            lower_bound,
            locations: self.locations,
            verify_bound,
        }
    }

    fn to_region_vid(&mut self, r: ty::Region<'tcx>) -> ty::RegionVid {
        if let ty::RePlaceholder(placeholder) = r {
            self.constraints
                .placeholder_region(self.infcx, *placeholder)
                .to_region_vid()
        } else {
            self.universal_regions.to_region_vid(r)
        }
    }

    fn add_outlives(&mut self, sup: ty::RegionVid, sub: ty::RegionVid) {
        self.constraints
            .outlives_constraints
            .push(OutlivesConstraint {
                locations: self.locations,
                category: self.category,
                sub,
                sup,
            });
    }

    fn add_type_test(&mut self, type_test: TypeTest<'tcx>) {
        debug!("add_type_test(type_test={:?})", type_test);
        self.constraints.type_tests.push(type_test);
    }
}

impl<'a, 'b, 'gcx, 'tcx> TypeOutlivesDelegate<'tcx>
    for &'a mut ConstraintConversion<'b, 'gcx, 'tcx>
{
    fn push_sub_region_constraint(
        &mut self,
        _origin: SubregionOrigin<'tcx>,
        a: ty::Region<'tcx>,
        b: ty::Region<'tcx>,
    ) {
        let b = self.to_region_vid(b);
        let a = self.to_region_vid(a);
        self.add_outlives(b, a);
    }

    fn push_verify(
        &mut self,
        _origin: SubregionOrigin<'tcx>,
        kind: GenericKind<'tcx>,
        a: ty::Region<'tcx>,
        bound: VerifyBound<'tcx>,
    ) {
        let type_test = self.verify_to_type_test(kind, a, bound);
        self.add_type_test(type_test);
    }
}
